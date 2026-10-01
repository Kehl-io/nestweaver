//! Daemon-owned Markdown link derivation records and admission.
use super::{
    ConnectionGuard, DaemonState, IndexedSearchMutationScope, MutationWorkerOwnership,
    establish_search_reconciliation_debt, finish_search_reconciliation, indexed_search_mutation,
    indexed_search_rows_before,
};
use nestweaver_engine::index_limits::NoteLimits;
use nestweaver_engine::index_md::MarkdownRefreshResult;
use nestweaver_engine::markdown_derivation::{
    self, CoverageIdentity, CoverageScope, DerivationPhase, DerivationRecords, RecordError,
    VaultDerivationRecord, admit_all_vaults, coverage_identity, expectation, filesystem_source,
    load_records, requires_current_derivation, save_records,
};
use nestweaver_schema::Vault;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tonic::Status;

fn note_limits(state: &DaemonState) -> NoteLimits {
    state
        .instance_cfg
        .as_ref()
        .map(|config| config.indexing.note_limits())
        .unwrap_or_default()
}

fn extra_ignore(_state: &DaemonState) -> Vec<String> {
    Vec::new()
}

fn cancelled(state: &DaemonState) -> bool {
    state.shutdown_started.load(Ordering::SeqCst)
}

/// The data instance derivation records are stamped and validated with.
///
/// nw-693: the LIVE identity, never the boot snapshot. A config-less daemon
/// that booted before the database recorded its instance snapshotted
/// `"default"` and stamped records with it; after a restart it adopted the
/// recorded instance and every record read as `ForeignRecord`.
fn record_instance(state: &DaemonState) -> String {
    state.effective_data_instance_id()
}

/// nw-693: heal records a config-less daemon stamped with the ambient
/// `"default"` before the database recorded its instance. Called only on the
/// WRITER paths (every stamp, demotion and migration, which a refresh runs
/// through), never from a read, because it rewrites the sidecar. Only toward
/// the instance the DATABASE carries; a `--config`-stated instance is intent
/// and is never rebound.
fn heal_ambient_default(
    state: &DaemonState,
    identity: &nestweaver_store::PublicationIdentity,
) -> Result<(), RecordError> {
    if state.instance_stated_by_config {
        return Ok(());
    }
    // The database's own instance: its record, else the single instance its
    // repos and vaults carry (`effective_data_instance_id`'s precedence).
    let recorded = record_instance(state);
    if markdown_derivation::rebind_ambient_default_records(&state.db_path, identity, &recorded)? {
        tracing::info!(
            instance = %recorded,
            "re-bound Markdown derivation records stamped with the ambient default instance"
        );
    }
    Ok(())
}

fn load_or_empty(
    state: &DaemonState,
    identity: &nestweaver_store::PublicationIdentity,
) -> Result<DerivationRecords, RecordError> {
    let instance = record_instance(state);
    let expected = expectation(identity, &instance);
    Ok(load_records(&state.db_path, &expected)?.unwrap_or_default())
}

fn persist(
    state: &DaemonState,
    identity: &nestweaver_store::PublicationIdentity,
    records: &DerivationRecords,
) -> Result<(), RecordError> {
    let instance = record_instance(state);
    save_records(&state.db_path, records, &expectation(identity, &instance))
}

/// Reuse the recorded coverage scope so IndexVault's FullRegisteredPolicy is
/// not remigrated as LegacyIndexedInventory, and a completed legacy migration
/// is not treated as SourceChanged by a Full-default admit.
fn coverage_for_vault(
    vault: &Vault,
    source: &markdown_derivation::SourceIdentity,
    extra: &[String],
    max_note_bytes: u64,
    records: Option<&DerivationRecords>,
    default_scope: CoverageScope,
) -> Result<CoverageIdentity, RecordError> {
    let scope = records
        .and_then(|records| records.vaults.get(&vault.uid))
        .map(|record| record.coverage.scope.clone())
        .unwrap_or(default_scope);
    coverage_identity(
        Path::new(&source.canonical_root),
        extra,
        max_note_bytes,
        scope,
    )
}

fn lookup_vault(state: &DaemonState, vault_path: &Path) -> anyhow::Result<Vault> {
    let canonical = vault_path.canonicalize()?;
    let vaults = state.store.list_vaults(None)?;
    vaults
        .into_iter()
        .find(|vault| {
            Path::new(&vault.root_path).canonicalize().ok().as_deref() == Some(canonical.as_path())
                || Path::new(&vault.root_path) == canonical.as_path()
        })
        .ok_or_else(|| anyhow::anyhow!("indexed vault is not present in the graph"))
}

/// The vault derivation this daemon is running right now, keyed by database
/// path so in-process test daemons over different databases stay apart.
///
/// `brain status` reads it without the write gate, so a derivation (which
/// holds that gate for its whole run) is disclosed while it runs instead of
/// status having nothing to say about the one thing the daemon is doing.
#[derive(Clone, Debug)]
struct DerivationInProgress {
    vault_uid: String,
    root_path: String,
    phase: &'static str,
    started_at_unix_seconds: u64,
    /// Notes the graph held for the vault when the run started; `None` when
    /// the count could not be read.
    indexed_notes: Option<usize>,
}

static IN_PROGRESS: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashMap<std::path::PathBuf, DerivationInProgress>>,
> = std::sync::LazyLock::new(Default::default);

/// Clears the in-progress entry when the run ends, however it ends.
struct InProgressGuard {
    db_path: std::path::PathBuf,
}

impl InProgressGuard {
    fn begin(state: &DaemonState, vault: &Vault) -> Self {
        let entry = DerivationInProgress {
            vault_uid: vault.uid.clone(),
            root_path: vault.root_path.clone(),
            phase: "recording",
            started_at_unix_seconds: now_unix_seconds(),
            indexed_notes: state.store.count_notes_in_vault(&vault.uid).ok(),
        };
        IN_PROGRESS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(state.db_path.clone(), entry);
        Self {
            db_path: state.db_path.clone(),
        }
    }

    fn phase(&self, phase: &'static str) {
        if let Some(entry) = IN_PROGRESS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get_mut(&self.db_path)
        {
            entry.phase = phase;
        }
        #[cfg(test)]
        tests::observe_phase(&self.db_path, phase);
    }
}

impl Drop for InProgressGuard {
    fn drop(&mut self) {
        IN_PROGRESS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&self.db_path);
    }
}

/// The `vault_derivation.in_progress` object, or null when nothing runs.
fn in_progress_json(state: &DaemonState) -> serde_json::Value {
    let entry = IN_PROGRESS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(&state.db_path)
        .cloned();
    match entry {
        Some(entry) => serde_json::json!({
            "vault_uid": entry.vault_uid,
            "root_path": entry.root_path,
            "phase": entry.phase,
            "started_at_unix_seconds": entry.started_at_unix_seconds,
            "elapsed_seconds": now_unix_seconds().saturating_sub(entry.started_at_unix_seconds),
            "indexed_notes": entry.indexed_notes,
        }),
        None => serde_json::Value::Null,
    }
}

const BLOCKED_RETRY_BASE_SECS: u64 = 30;
const BLOCKED_RETRY_MAX_SECS: u64 = 60 * 60;

fn now_unix_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Every retry of a Blocked vault is a full derivation refresh under the
/// writer, so the schedule doubles per consecutive failure up to an hour.
fn blocked_retry_delay(attempts: u32) -> u64 {
    let doublings = attempts.saturating_sub(1).min(16);
    BLOCKED_RETRY_BASE_SECS
        .saturating_mul(1 << doublings)
        .min(BLOCKED_RETRY_MAX_SECS)
}

fn blocked_retry_due(records: Option<&DerivationRecords>, vault_uid: &str) -> bool {
    records
        .and_then(|records| records.vaults.get(vault_uid))
        .is_some_and(|record| markdown_derivation::blocked_retry_due(record, now_unix_seconds()))
}

fn block_record(record: &mut VaultDerivationRecord, reason: markdown_derivation::BlockedReason) {
    block_record_at(record, reason, now_unix_seconds());
}

fn block_record_at(
    record: &mut VaultDerivationRecord,
    reason: markdown_derivation::BlockedReason,
    now_unix_seconds: u64,
) {
    record.phase = DerivationPhase::Blocked;
    record.last_error = Some(reason);
    record.attempts = record.attempts.saturating_add(1);
    record.retry_after_unix_seconds =
        Some(now_unix_seconds.saturating_add(blocked_retry_delay(record.attempts)));
    record.witness = None;
    record.pending_generation = None;
    record.completed_generation = None;
}

fn stamp_from_refresh(
    state: &DaemonState,
    vault: &Vault,
    extra: &[String],
    max_note_bytes: u64,
    scope: CoverageScope,
    result: &MarkdownRefreshResult,
) -> anyhow::Result<()> {
    if result
        .index
        .skipped
        .iter()
        .any(markdown_derivation::is_coverage_gap)
        || result.publication.disposition
            != nestweaver_engine::manifest::GraphMutationPublicationDisposition::CommittedComplete
    {
        anyhow::bail!("vault derivation requires complete publication coverage");
    }
    let identity = state
        .store
        .publication_identity()?
        .ok_or_else(|| anyhow::anyhow!("graph publication identity is absent"))?;
    let source = filesystem_source(Path::new(&vault.root_path))?;
    let coverage = coverage_identity(
        Path::new(&source.canonical_root),
        extra,
        max_note_bytes,
        scope,
    )?;
    let notes = state.store.list_notes(Some(&vault.uid))?;
    heal_ambient_default(state, &identity)?;
    let mut records = load_or_empty(state, &identity)?;
    markdown_derivation::record_full_refresh(
        &mut records,
        vault,
        source,
        coverage,
        result,
        &notes,
        &identity,
    )?;
    persist(state, &identity, &records)?;
    Ok(())
}

/// What IndexVault's derivation stamp did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum IndexStamp {
    /// Derivation is Current.
    Current,
    /// The refresh disclosed a coverage GAP (a failure skip: an unreadable
    /// directory or note, a parse error), so the record was persisted Blocked
    /// instead of Current. The graph and its skip rows are committed.
    WithheldForCoverageGap,
}

/// Stamp IndexVault's derivation, or withhold it over a coverage gap.
///
/// nw-651 (DECISION 2026-09-24): a failure skip is DISCLOSED, not a hard
/// failure — IndexVault completes with degraded coverage — and derivation must
/// not read Current while it exists. Refusing by returning an error (the
/// pre-nw-651 shape) failed the whole RPC AND left whatever record was there
/// before untouched: a vault stamped Current yesterday stayed Current over
/// today's unreadable subdirectory. The record is therefore written Blocked
/// (`PublicationIncomplete`), so `brain status` counts it, gated tools refuse,
/// and the background loop retries it on the Blocked backoff — its strict walk
/// fails closed until the directory is readable, then stamps Current.
///
/// What the graph holds meanwhile: every route RETAINS already-indexed notes
/// under an unreadable directory (the full index hands off to the `--since`
/// route rather than replacing the vault). They are kept but not refreshed,
/// and their links into rewritten notes may be stale — which is precisely what
/// "not Current" tells the link-graph tools.
pub(super) fn stamp_index_success(
    state: &DaemonState,
    vault_path: &Path,
    extra: &[String],
    max_note_bytes: u64,
    result: &MarkdownRefreshResult,
) -> anyhow::Result<IndexStamp> {
    if withhold_if_coverage_gap(
        state,
        vault_path,
        extra,
        max_note_bytes,
        &result.index.skipped,
    )? {
        return Ok(IndexStamp::WithheldForCoverageGap);
    }
    let vault = lookup_vault(state, vault_path)?;
    stamp_from_refresh(
        state,
        &vault,
        extra,
        max_note_bytes,
        CoverageScope::FullRegisteredPolicy,
        result,
    )?;
    Ok(IndexStamp::Current)
}

/// Persist the vault's derivation Blocked when `skipped` holds a coverage
/// GAP; `Ok(true)` when it did. ONE rule for every daemon route that commits
/// vault content without stamping it: IndexVault (via
/// [`stamp_index_success`]) and RefreshVaultSince.
///
/// nw-651: RefreshVaultSince admits a Current vault through `ensure_current`
/// and never touched the record afterwards, so a watcher- or RPC-driven
/// refresh that disclosed an unreadable directory left the vault Current —
/// contradicting the decision that derivation must not read Current while
/// such a row exists. A refresh never STAMPS Current, so this only ever
/// demotes; a clean refresh leaves the record exactly as it was.
pub(super) fn withhold_if_coverage_gap(
    state: &DaemonState,
    vault_path: &Path,
    extra: &[String],
    max_note_bytes: u64,
    skipped: &[nestweaver_parser::SkippedFile],
) -> anyhow::Result<bool> {
    if !skipped.iter().any(markdown_derivation::is_coverage_gap) {
        return Ok(false);
    }
    let vault = lookup_vault(state, vault_path)?;
    withhold_for_coverage_gap(state, &vault, extra, max_note_bytes)?;
    Ok(true)
}

fn withhold_for_coverage_gap(
    state: &DaemonState,
    vault: &Vault,
    extra: &[String],
    max_note_bytes: u64,
) -> anyhow::Result<()> {
    let identity = state
        .store
        .publication_identity()?
        .ok_or_else(|| anyhow::anyhow!("graph publication identity is absent"))?;
    let source = filesystem_source(Path::new(&vault.root_path))?;
    let coverage = coverage_identity(
        Path::new(&source.canonical_root),
        extra,
        max_note_bytes,
        CoverageScope::FullRegisteredPolicy,
    )?;
    heal_ambient_default(state, &identity)?;
    let mut records = load_or_empty(state, &identity)?;
    let mut record = records
        .vaults
        .remove(&vault.uid)
        .unwrap_or_else(|| VaultDerivationRecord::pending(vault, source.clone(), coverage.clone()));
    record.source = source;
    record.coverage = coverage;
    record.derivation_version = markdown_derivation::DERIVATION_VERSION;
    block_record(
        &mut record,
        markdown_derivation::BlockedReason::PublicationIncomplete,
    );
    records.vaults.insert(vault.uid.clone(), record);
    persist(state, &identity, &records)?;
    Ok(())
}

fn migrate_vault(
    state: &DaemonState,
    vault: &Vault,
    extra: &[String],
    max_note_bytes: u64,
    default_scope: CoverageScope,
) -> anyhow::Result<()> {
    if cancelled(state) {
        anyhow::bail!("vault derivation cancelled");
    }
    let progress = InProgressGuard::begin(state, vault);
    let identity = state
        .store
        .publication_identity()?
        .ok_or_else(|| anyhow::anyhow!("graph publication identity is absent"))?;
    let source = filesystem_source(Path::new(&vault.root_path))?;
    if nestweaver_schema::vault_uid(&vault.instance_id, &source.canonical_root) != vault.uid {
        anyhow::bail!("vault source identity does not match its UID");
    }
    heal_ambient_default(state, &identity)?;
    let mut records = load_or_empty(state, &identity)?;
    let coverage = coverage_for_vault(
        vault,
        &source,
        extra,
        max_note_bytes,
        Some(&records),
        default_scope,
    )?;
    let mut record = records
        .vaults
        .remove(&vault.uid)
        .unwrap_or_else(|| VaultDerivationRecord::pending(vault, source.clone(), coverage.clone()));
    record.source = source.clone();
    record.coverage = coverage.clone();
    // This attempt targets the current version. Keeping the old one would
    // let OlderVersion (retryable, checked before the phase) bypass the
    // Blocked backoff if the attempt fails.
    record.derivation_version = markdown_derivation::DERIVATION_VERSION;
    record.phase = DerivationPhase::Pending;
    record.witness = None;
    record.pending_generation = None;
    record.completed_generation = None;
    records.vaults.insert(vault.uid.clone(), record.clone());
    persist(state, &identity, &records)?;

    progress.phase("refreshing notes");
    let admission = establish_search_reconciliation_debt(state, "vault_derivation")?;
    let indexed_before = indexed_search_rows_before(state);
    let reader = nestweaver_engine::index_md::filesystem_vault_reader(
        Path::new(&source.canonical_root),
        note_limits(state),
    )
    .strict_enumeration();
    let refresh = nestweaver_engine::index_md::refresh_indexed_markdown_derivation(
        &reader,
        &state.store,
        vault,
        || cancelled(state),
    );
    let result = match refresh {
        Ok(result) => result,
        Err(error) => {
            let mut records = load_or_empty(state, &identity)?;
            if let Some(record) = records.vaults.get_mut(&vault.uid) {
                block_record(
                    record,
                    markdown_derivation::BlockedReason::SourceUnavailable,
                );
            }
            persist(state, &identity, &records)?;
            return Err(error);
        }
    };
    progress.phase("reconciling search");
    let mutation = indexed_search_mutation(
        indexed_before,
        &state.store,
        IndexedSearchMutationScope::MayIncludeVaultFiles,
    );
    finish_search_reconciliation(state, mutation, "vault_derivation", admission)?;
    progress.phase("stamping");
    if let Err(error) =
        stamp_from_refresh(state, vault, extra, max_note_bytes, coverage.scope, &result)
    {
        // Left Pending, the record would be admitted as retryable again at
        // once and the background loop would rerun a full refresh every few
        // seconds. Blocked retries on the backoff schedule instead.
        let mut records = load_or_empty(state, &identity)?;
        if let Some(record) = records.vaults.get_mut(&vault.uid) {
            block_record(
                record,
                markdown_derivation::BlockedReason::PublicationIncomplete,
            );
        }
        persist(state, &identity, &records)?;
        return Err(error);
    }
    Ok(())
}

pub(super) fn ensure_current(
    state: &DaemonState,
    vault_path: &Path,
    extra: &[String],
    max_note_bytes: u64,
) -> anyhow::Result<()> {
    if state.read_only {
        anyhow::bail!("read-only replica cannot migrate vault derivation");
    }
    let vault = lookup_vault(state, vault_path)?;
    let identity = state
        .store
        .publication_identity()?
        .ok_or_else(|| anyhow::anyhow!("graph publication identity is absent"))?;
    let source = filesystem_source(Path::new(&vault.root_path))?;
    heal_ambient_default(state, &identity)?;
    let instance = record_instance(state);
    let records = load_records(&state.db_path, &expectation(&identity, &instance))?;
    let coverage = coverage_for_vault(
        &vault,
        &source,
        extra,
        max_note_bytes,
        records.as_ref(),
        CoverageScope::FullRegisteredPolicy,
    )?;
    match markdown_derivation::admit_vault(
        records.as_ref(),
        &expectation(&identity, &instance),
        &vault,
        &source,
        &coverage,
        false,
    ) {
        Ok(()) => Ok(()),
        Err(error) if error.retryable || blocked_retry_due(records.as_ref(), &vault.uid) => {
            migrate_vault(
                state,
                &vault,
                extra,
                max_note_bytes,
                CoverageScope::FullRegisteredPolicy,
            )
        }
        Err(error) => {
            let blocked = records
                .as_ref()
                .and_then(|records| records.vaults.get(&vault.uid))
                .filter(|record| record.phase == DerivationPhase::Blocked);
            match blocked {
                // The vault-specific remedy replaces the generic path-free
                // one in `DerivationUnavailable`'s Display, so it is said once.
                Some(record) => Err(anyhow::anyhow!(
                    "Markdown link derivation is not current ({:?}): {}",
                    error.reason,
                    blocked_vault_remedy(state, &vault, record, now_unix_seconds())
                )),
                None => Err(error.into()),
            }
        }
    }
}

/// nw-694: what a refusal over a Blocked vault must say. The bare
/// "Markdown link derivation is not current (SourceBlocked)" named neither the
/// directory nor the way out, and the refusal repeats until the backoff
/// expires even after the directory is fixed. It now names the vault, the
/// directories that cannot be read NOW (one non-strict walk, only on this
/// refusal path), when the automatic retry is due, and the remedy: a FULL
/// refresh re-derives the vault at once, because IndexVault does not wait on
/// the Blocked backoff.
fn blocked_vault_remedy(
    state: &DaemonState,
    vault: &Vault,
    record: &VaultDerivationRecord,
    now_unix_seconds: u64,
) -> String {
    use nestweaver_engine::content_reader::{ContentReader, UNREADABLE_DIR_REASON};
    let reader = nestweaver_engine::index_md::filesystem_vault_reader(
        Path::new(&vault.root_path),
        note_limits(state),
    );
    let unreadable: Vec<String> = match reader.list_files() {
        Ok(_) => reader
            .skipped_dirs()
            .into_iter()
            .filter(|dir| dir.reason == UNREADABLE_DIR_REASON)
            .map(|dir| dir.path)
            .collect(),
        Err(error) => vec![format!("the vault root itself ({error})")],
    };
    let blocked_on = if unreadable.is_empty() {
        "no directory is unreadable now".to_string()
    } else {
        format!("unreadable now: {}", unreadable.join(", "))
    };
    let retry = match record.retry_after_unix_seconds {
        Some(at) if at > now_unix_seconds => format!(
            "the automatic retry is due in {}s (unix time {at})",
            at - now_unix_seconds
        ),
        _ => "the automatic retry is due now".to_string(),
    };
    format!(
        "vault {} is blocked ({:?}; {blocked_on}); {retry}. To re-derive it now, fix the \
         directory and run a full `nestweaver brain refresh {}` (without --since)",
        vault.root_path,
        record
            .last_error
            .unwrap_or(markdown_derivation::BlockedReason::SourceUnavailable),
        vault.root_path,
    )
}

pub(super) fn admit_tool(state: &DaemonState, tool: &str) -> Result<(), Status> {
    if !requires_current_derivation(tool) {
        return Ok(());
    }
    // In-memory test graphs use a non-durable db_path marker and have no
    // derivation sidecar. Fail-closing those RPCs would mask authorization
    // refusals with RecordUnavailable.
    if state.db_path == Path::new(":memory:") {
        return Ok(());
    }
    admit_all_vaults(
        &state.store,
        &state.db_path,
        &record_instance(state),
        &extra_ignore(state),
        note_limits(state).max_note_bytes(),
        state.read_only,
    )
    .map_err(|error| Status::failed_precondition(error.to_string()))
}

pub(super) fn status_overlay(state: &DaemonState, value: &mut serde_json::Value) {
    let identity = match state.store.publication_identity() {
        Ok(Some(identity)) => identity,
        _ => return,
    };
    let instance = record_instance(state);
    let records = load_records(&state.db_path, &expectation(&identity, &instance))
        .ok()
        .flatten();
    let in_progress = in_progress_json(state);
    // A derivation that runs before any record exists (the sidecar was lost)
    // is still disclosed.
    let Some(records) =
        records.or_else(|| (!in_progress.is_null()).then(DerivationRecords::default))
    else {
        return;
    };
    let pending = records
        .vaults
        .values()
        .filter(|record| record.phase != DerivationPhase::Current)
        .count()
        .max(usize::from(!in_progress.is_null()));
    // nw-694: name the Blocked vaults, so the text render can say a vault is
    // blocked rather than leaving it to a JSON-only count.
    let blocked: Vec<serde_json::Value> = records
        .vaults
        .values()
        .filter(|record| record.phase == DerivationPhase::Blocked)
        .map(|record| {
            serde_json::json!({
                "vault_uid": record.vault_uid,
                "root_path": record.source.canonical_root,
                "reason": record.last_error,
                "retry_after_unix_seconds": record.retry_after_unix_seconds,
            })
        })
        .collect();
    if let serde_json::Value::Object(object) = value {
        object.insert(
            "vault_derivation".to_string(),
            serde_json::json!({
                "expected_version": markdown_derivation::DERIVATION_VERSION,
                "pending_or_blocked_vaults": pending,
                "blocked_vaults": blocked,
                "in_progress": in_progress,
                "read_only": state.read_only,
            }),
        );
    }
}

pub(super) async fn run(state: Arc<DaemonState>) {
    if state.read_only {
        return;
    }
    let mut shutdown = state.shutdown_tx.subscribe();
    let mut delay = 0u64;
    loop {
        if state.shutdown_started.load(Ordering::SeqCst) || *shutdown.borrow() {
            break;
        }
        tokio::select! {
            _ = shutdown.changed() => break,
            _ = tokio::time::sleep(Duration::from_secs(delay)) => {},
        }
        if state.shutdown_started.load(Ordering::SeqCst) {
            break;
        }
        let inspect = Arc::clone(&state);
        let next = tokio::task::spawn_blocking(move || inspect_next(&inspect)).await;
        let Some(vault_uid) = next.ok().and_then(|result| result.ok()).flatten() else {
            delay = 2;
            continue;
        };
        let guard = match ConnectionGuard::write(&state) {
            Ok(guard) => guard,
            Err(_) => break,
        };
        let lease = state.write_gate.lock("vault_derivation").await;
        let work = Arc::clone(&state);
        let result = tokio::task::spawn_blocking(move || {
            let _ownership = MutationWorkerOwnership {
                _write_lease: lease,
                _connection_guard: guard,
            };
            migrate_named(&work, &vault_uid)
        })
        .await;
        delay = match result {
            Ok(Ok(())) => 0,
            _ => 4,
        };
    }
}

pub(super) fn inspect_next(state: &DaemonState) -> anyhow::Result<Option<String>> {
    let identity = match state.store.publication_identity()? {
        Some(identity) => identity,
        None => return Ok(None),
    };
    // nw-693 review (M1): a sidecar an older daemon stamped with the ambient
    // "default" failed this load with ForeignIdentity, which the loop dropped
    // and retried every 2s forever. It is due: `migrate_named` heals it under
    // the write lease, so an upgrade heals without a manual refresh.
    if !state.instance_stated_by_config
        && markdown_derivation::records_await_ambient_rebind(
            &state.db_path,
            &identity,
            &record_instance(state),
        )
    {
        return Ok(state
            .store
            .list_vaults(None)?
            .into_iter()
            .next()
            .map(|vault| vault.uid));
    }
    let records = load_or_empty(state, &identity)?;
    let extra = extra_ignore(state);
    let max_note_bytes = note_limits(state).max_note_bytes();
    for vault in state.store.list_vaults(None)? {
        let Ok(source) = filesystem_source(Path::new(&vault.root_path)) else {
            continue;
        };
        let Ok(coverage) = coverage_for_vault(
            &vault,
            &source,
            &extra,
            max_note_bytes,
            Some(&records),
            CoverageScope::LegacyIndexedInventory,
        ) else {
            continue;
        };
        if markdown_derivation::admit_vault(
            Some(&records),
            &expectation(&identity, &record_instance(state)),
            &vault,
            &source,
            &coverage,
            false,
        )
        .is_err_and(|error| error.retryable || blocked_retry_due(Some(&records), &vault.uid))
        {
            return Ok(Some(vault.uid));
        }
    }
    Ok(None)
}

pub(super) fn migrate_named(state: &DaemonState, vault_uid: &str) -> anyhow::Result<()> {
    let vault = state
        .store
        .list_vaults(None)?
        .into_iter()
        .find(|vault| vault.uid == vault_uid)
        .ok_or_else(|| anyhow::anyhow!("vault disappeared before derivation"))?;
    let identity = state
        .store
        .publication_identity()?
        .ok_or_else(|| anyhow::anyhow!("graph publication identity is absent"))?;
    let source = filesystem_source(Path::new(&vault.root_path))?;
    let extra = extra_ignore(state);
    let max_note_bytes = note_limits(state).max_note_bytes();
    heal_ambient_default(state, &identity)?;
    let records = load_or_empty(state, &identity)?;
    let coverage = coverage_for_vault(
        &vault,
        &source,
        &extra,
        max_note_bytes,
        Some(&records),
        CoverageScope::LegacyIndexedInventory,
    )?;
    match markdown_derivation::admit_vault(
        Some(&records),
        &expectation(&identity, &record_instance(state)),
        &vault,
        &source,
        &coverage,
        false,
    ) {
        Ok(()) => Ok(()),
        Err(error) if error.retryable || blocked_retry_due(Some(&records), &vault.uid) => {
            migrate_vault(
                state,
                &vault,
                &extra,
                max_note_bytes,
                CoverageScope::LegacyIndexedInventory,
            )
        }
        Err(error) => Err(error.into()),
    }
}

pub(super) fn http_max_note_bytes(state: &DaemonState) -> u64 {
    note_limits(state).max_note_bytes()
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use nestweaver_schema::vault_uid;
    use std::collections::BTreeMap;

    type PhaseHook = Box<dyn Fn(&'static str)>;

    thread_local! {
        static PHASE_HOOK: std::cell::RefCell<Option<PhaseHook>> =
            const { std::cell::RefCell::new(None) };
    }

    /// Run `hook` on this thread at every derivation phase change, so a test
    /// can observe status from inside a real migration.
    pub(in super::super) fn set_phase_hook(hook: Option<PhaseHook>) {
        PHASE_HOOK.with(|slot| *slot.borrow_mut() = hook);
    }

    pub(super) fn observe_phase(_db_path: &Path, phase: &'static str) {
        PHASE_HOOK.with(|slot| {
            if let Some(hook) = slot.borrow().as_ref() {
                hook(phase);
            }
        });
    }

    #[test]
    fn coverage_for_vault_does_not_downgrade_recorded_full_to_legacy() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let vault = Vault {
            uid: vault_uid("brain", root.to_str().unwrap()),
            name: "vault".into(),
            root_path: root.display().to_string(),
            instance_id: "brain".into(),
        };
        let source = markdown_derivation::SourceIdentity {
            provider: markdown_derivation::SourceProvider::Filesystem,
            canonical_root: root.display().to_string(),
            provider_repo_uid: None,
        };
        let full = coverage_identity(root, &[], 1024, CoverageScope::FullRegisteredPolicy).unwrap();
        let records = DerivationRecords {
            vaults: BTreeMap::from([(
                vault.uid.clone(),
                VaultDerivationRecord::pending(&vault, source.clone(), full.clone()),
            )]),
        };
        let coverage = coverage_for_vault(
            &vault,
            &source,
            &[],
            1024,
            Some(&records),
            CoverageScope::LegacyIndexedInventory,
        )
        .unwrap();
        assert_eq!(coverage.scope, CoverageScope::FullRegisteredPolicy);
        assert_eq!(coverage.policy_digest, full.policy_digest);
    }

    #[test]
    fn block_record_backs_off_exponentially_to_a_cap() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let vault = Vault {
            uid: vault_uid("brain", root.to_str().unwrap()),
            name: "vault".into(),
            root_path: root.display().to_string(),
            instance_id: "brain".into(),
        };
        let source = markdown_derivation::SourceIdentity {
            provider: markdown_derivation::SourceProvider::Filesystem,
            canonical_root: root.display().to_string(),
            provider_repo_uid: None,
        };
        let coverage =
            coverage_identity(root, &[], 1024, CoverageScope::LegacyIndexedInventory).unwrap();
        let mut record = VaultDerivationRecord::pending(&vault, source, coverage);
        let delays: Vec<u64> = (0..10)
            .map(|_| {
                block_record_at(
                    &mut record,
                    markdown_derivation::BlockedReason::SourceUnavailable,
                    1_000,
                );
                record.retry_after_unix_seconds.unwrap() - 1_000
            })
            .collect();
        assert_eq!(delays, [30, 60, 120, 240, 480, 960, 1920, 3600, 3600, 3600]);
        assert_eq!(record.attempts, 10);
        assert_eq!(record.phase, DerivationPhase::Blocked);
    }
}
