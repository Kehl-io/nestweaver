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
    inventory_digest, load_records, requires_current_derivation, save_records,
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

fn load_or_empty(
    state: &DaemonState,
    identity: &nestweaver_store::PublicationIdentity,
) -> Result<DerivationRecords, RecordError> {
    let expected = expectation(identity, &state.data_instance_id);
    Ok(load_records(&state.db_path, &expected)?.unwrap_or_default())
}

fn persist(
    state: &DaemonState,
    identity: &nestweaver_store::PublicationIdentity,
    records: &DerivationRecords,
) -> Result<(), RecordError> {
    save_records(
        &state.db_path,
        records,
        &expectation(identity, &state.data_instance_id),
    )
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
    let mut records = load_or_empty(state, &identity)?;
    let mut record = records
        .vaults
        .remove(&vault.uid)
        .unwrap_or_else(|| VaultDerivationRecord::pending(vault, source.clone(), coverage.clone()));
    record.source = source;
    record.coverage = coverage;
    record.derivation_version = markdown_derivation::DERIVATION_VERSION;
    record.phase = DerivationPhase::Pending;
    record.witness = None;
    record.pending_generation = None;
    record.completed_generation = None;
    record.last_error = None;
    record.retry_after_unix_seconds = None;
    record.attempts = 0;
    record.record_complete_graph(
        result,
        result.index.notes_count,
        inventory_digest(&notes),
        &identity,
    )?;
    record.record_search_reconciled(&identity)?;
    records.vaults.insert(vault.uid.clone(), record);
    persist(state, &identity, &records)?;
    Ok(())
}

pub(super) fn stamp_index_success(
    state: &DaemonState,
    vault_path: &Path,
    extra: &[String],
    max_note_bytes: u64,
    result: &MarkdownRefreshResult,
) -> anyhow::Result<()> {
    let vault = lookup_vault(state, vault_path)?;
    stamp_from_refresh(
        state,
        &vault,
        extra,
        max_note_bytes,
        CoverageScope::FullRegisteredPolicy,
        result,
    )
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
    let identity = state
        .store
        .publication_identity()?
        .ok_or_else(|| anyhow::anyhow!("graph publication identity is absent"))?;
    let source = filesystem_source(Path::new(&vault.root_path))?;
    if nestweaver_schema::vault_uid(&vault.instance_id, &source.canonical_root) != vault.uid {
        anyhow::bail!("vault source identity does not match its UID");
    }
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
    record.phase = DerivationPhase::Pending;
    record.witness = None;
    record.pending_generation = None;
    record.completed_generation = None;
    records.vaults.insert(vault.uid.clone(), record.clone());
    persist(state, &identity, &records)?;

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
    let mutation = indexed_search_mutation(
        indexed_before,
        &state.store,
        IndexedSearchMutationScope::MayIncludeVaultFiles,
    );
    finish_search_reconciliation(state, mutation, "vault_derivation", admission)?;
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
    let records = load_records(
        &state.db_path,
        &expectation(&identity, &state.data_instance_id),
    )?;
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
        &expectation(&identity, &state.data_instance_id),
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
        Err(error) => Err(error.into()),
    }
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
        &state.data_instance_id,
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
    let records = load_records(
        &state.db_path,
        &expectation(&identity, &state.data_instance_id),
    )
    .ok()
    .flatten();
    let Some(records) = records else {
        return;
    };
    let pending = records
        .vaults
        .values()
        .filter(|record| record.phase != DerivationPhase::Current)
        .count();
    if let serde_json::Value::Object(object) = value {
        object.insert(
            "vault_derivation".to_string(),
            serde_json::json!({
                "expected_version": markdown_derivation::DERIVATION_VERSION,
                "pending_or_blocked_vaults": pending,
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
            &expectation(&identity, &state.data_instance_id),
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
        &expectation(&identity, &state.data_instance_id),
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
mod tests {
    use super::*;
    use nestweaver_schema::vault_uid;
    use std::collections::BTreeMap;

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
