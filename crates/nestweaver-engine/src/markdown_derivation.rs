//! Durable evidence for the algorithm that produced a vault's Markdown links.
//!
//! This version is independent of manifest snapshots, package versions and
//! code resolver generations. Only the daemon's admitted writer may persist
//! transitions. Loading and admission never mutate graph or sidecar state.
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};

use nestweaver_schema::Vault;
use nestweaver_store::PublicationIdentity;
use serde::{Deserialize, Serialize};

pub const RECORD_SCHEMA_VERSION: u32 = 1;
pub const DERIVATION_VERSION: u32 = crate::index_md::MARKDOWN_LINK_DERIVATION_VERSION;
const MAX_RECORD_BYTES: usize = 8 * 1024 * 1024;
const MAX_VAULT_RECORDS: usize = 10_000;
const RECORD_SUFFIX: &str = ".markdown-derivation.json";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceProvider {
    Filesystem,
    ManagedBare,
    #[serde(other)]
    Unsupported,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceIdentity {
    pub provider: SourceProvider,
    /// Canonical identity used when deriving the Vault UID. A bare provider
    /// must resolve this through its managed source registration, never by
    /// treating the clone directory as an ordinary filesystem vault.
    pub canonical_root: String,
    pub provider_repo_uid: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CoverageScope {
    LegacyIndexedInventory,
    FullRegisteredPolicy,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CoverageIdentity {
    pub scope: CoverageScope,
    /// Hash of the explicit provider/eligibility/ignore policy. This is not
    /// inferred from the current contents of a legacy vault.
    pub policy_digest: String,
    pub max_note_bytes: u64,
    pub extra_ignore_patterns: Vec<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DerivationPhase {
    Pending,
    Running,
    GraphCommittedAwaitingReconciliation,
    Current,
    Blocked,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BlockedReason {
    SourceUnavailable,
    SourcePolicyChanged,
    ProviderUnavailable,
    InvalidInventory,
    PreparationLimit,
    PublicationIncomplete,
    SearchUnavailable,
    PersistenceFailed,
    Cancelled,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompletionWitness {
    pub publication_identity: PublicationIdentity,
    pub vault_uid: String,
    pub source: SourceIdentity,
    pub coverage: CoverageIdentity,
    pub derivation_version: u32,
    pub source_inventory_digest: String,
    pub notes_count: u64,
    pub graph_generation: u64,
    pub search_reconciled: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VaultDerivationRecord {
    pub vault_uid: String,
    pub data_instance_id: String,
    pub source: SourceIdentity,
    pub coverage: CoverageIdentity,
    pub derivation_version: u32,
    pub phase: DerivationPhase,
    pub attempts: u32,
    pub last_error: Option<BlockedReason>,
    pub retry_after_unix_seconds: Option<u64>,
    pub pending_generation: Option<u64>,
    /// Audit only: unrelated graph generations do not invalidate Current.
    pub completed_generation: Option<u64>,
    pub witness: Option<CompletionWitness>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct DerivationRecords {
    pub vaults: BTreeMap<String, VaultDerivationRecord>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RecordEnvelope {
    schema_version: u32,
    identity: PublicationIdentity,
    data_instance_id: String,
    payload_checksum: String,
    payload: serde_json::Value,
}

pub struct RecordExpectation<'a> {
    pub identity: &'a PublicationIdentity,
    /// Primary database instance. Individual vaults retain their own instance
    /// identity (including imported vaults), verified again at scoped admission.
    pub data_instance_id: &'a str,
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum RecordError {
    #[error("Markdown derivation record exceeds its bounded inventory limit")]
    LimitExceeded,
    #[error("Markdown derivation record is corrupt: {0}")]
    Corrupt(&'static str),
    #[error("Markdown derivation record belongs to another data identity")]
    ForeignIdentity,
    #[error("Markdown derivation record has unsupported schema {0}")]
    UnsupportedSchema(u64),
    #[error("Markdown derivation version {0} is newer than this reader")]
    FutureVersion(u32),
    #[error("Markdown derivation source provider is unsupported")]
    UnsupportedProvider,
    #[error("Markdown derivation identity could not be verified")]
    InvalidIdentity,
    #[error("Markdown derivation requires complete graph publication and source coverage")]
    IncompletePublication,
    #[error("Markdown derivation record I/O failed ({0:?})")]
    Io(std::io::ErrorKind),
    /// nw-684: the vault's `.brainignore` exists but cannot be read. Named so
    /// the operator can fix it; admission still reports the path-free
    /// `RecordUnavailable`.
    #[error(
        "cannot read {} ({kind:?}) — make it readable (or remove it to use the defaults) \
         before this vault can be indexed or admitted",
        path.display()
    )]
    IgnorePolicyUnreadable {
        path: std::path::PathBuf,
        kind: std::io::ErrorKind,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AdmissionReason {
    MissingRecord,
    OlderVersion,
    MigrationPending,
    ReconciliationPending,
    SourceChanged,
    SourceBlocked,
    UpstreamRequired,
    RecordInvalid,
    ForeignRecord,
    UnsupportedSchema,
    FutureVersion,
    UnsupportedProvider,
    RecordLimit,
    RecordUnavailable,
}

/// Safe across authorization boundaries: no vault UID, source path, stored
/// error text, or hidden-vault count appears in the admission error.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, thiserror::Error)]
#[error("Markdown link derivation is not current ({reason:?})")]
pub struct DerivationUnavailable {
    pub code: &'static str,
    pub reason: AdmissionReason,
    pub retryable: bool,
    pub expected_version: u32,
}

impl DerivationUnavailable {
    fn pending(reason: AdmissionReason, read_only: bool) -> Self {
        Self {
            code: "vault_derivation_pending",
            reason: if read_only {
                AdmissionReason::UpstreamRequired
            } else {
                reason
            },
            retryable: !read_only,
            expected_version: DERIVATION_VERSION,
        }
    }
    pub fn from_record_error(error: &RecordError) -> Self {
        Self {
            code: "vault_derivation_unavailable",
            reason: match error {
                RecordError::ForeignIdentity => AdmissionReason::ForeignRecord,
                RecordError::UnsupportedSchema(_) => AdmissionReason::UnsupportedSchema,
                RecordError::FutureVersion(_) => AdmissionReason::FutureVersion,
                RecordError::UnsupportedProvider => AdmissionReason::UnsupportedProvider,
                RecordError::LimitExceeded => AdmissionReason::RecordLimit,
                RecordError::Io(_) | RecordError::IgnorePolicyUnreadable { .. } => {
                    AdmissionReason::RecordUnavailable
                }
                _ => AdmissionReason::RecordInvalid,
            },
            retryable: false,
            expected_version: DERIVATION_VERSION,
        }
    }
}

impl From<RecordError> for DerivationUnavailable {
    fn from(error: RecordError) -> Self {
        Self::from_record_error(&error)
    }
}

pub fn record_path(db_path: &Path) -> PathBuf {
    crate::sidecar_path(db_path, RECORD_SUFFIX)
}

fn digest_valid(digest: &str) -> bool {
    digest.len() == 64
        && digest
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn check_expectation(expected: &RecordExpectation<'_>) -> Result<(), RecordError> {
    expected
        .identity
        .validate()
        .map_err(|_| RecordError::InvalidIdentity)?;
    if expected.data_instance_id.trim().is_empty() {
        return Err(RecordError::InvalidIdentity);
    }
    Ok(())
}

fn check_source(source: &SourceIdentity) -> Result<(), RecordError> {
    let root = Path::new(&source.canonical_root);
    if !root.is_absolute()
        || root
            .components()
            .any(|c| matches!(c, Component::ParentDir | Component::CurDir))
    {
        return Err(RecordError::Corrupt("source root is not canonical"));
    }
    match source.provider {
        SourceProvider::Filesystem if source.provider_repo_uid.is_none() => Ok(()),
        SourceProvider::ManagedBare
            if source
                .provider_repo_uid
                .as_ref()
                .is_some_and(|uid| !uid.trim().is_empty()) =>
        {
            Ok(())
        }
        SourceProvider::Unsupported => Err(RecordError::UnsupportedProvider),
        _ => Err(RecordError::Corrupt(
            "source/provider binding is incomplete",
        )),
    }
}

fn check_record(
    record: &VaultDerivationRecord,
    identity: &PublicationIdentity,
) -> Result<(), RecordError> {
    identity
        .validate()
        .map_err(|_| RecordError::InvalidIdentity)?;
    check_source(&record.source)?;
    if record.data_instance_id.trim().is_empty()
        || nestweaver_schema::vault_uid(&record.data_instance_id, &record.source.canonical_root)
            != record.vault_uid
    {
        return Err(RecordError::Corrupt(
            "vault source identity does not match its UID",
        ));
    }
    if !digest_valid(&record.coverage.policy_digest) || record.coverage.max_note_bytes == 0 {
        return Err(RecordError::Corrupt("coverage policy is incomplete"));
    }
    if let Some(witness) = &record.witness
        && (&witness.publication_identity != identity
            || witness.vault_uid != record.vault_uid
            || witness.source != record.source
            || witness.coverage != record.coverage
            || witness.derivation_version != record.derivation_version
            || !digest_valid(&witness.source_inventory_digest))
    {
        return Err(RecordError::Corrupt(
            "completion witness does not match its record",
        ));
    }
    match record.phase {
        DerivationPhase::Current => {
            let witness = record
                .witness
                .as_ref()
                .ok_or(RecordError::Corrupt("Current has no completion witness"))?;
            if !witness.search_reconciled
                || record.completed_generation != Some(witness.graph_generation)
                || record.pending_generation.is_some()
                || record.last_error.is_some()
                || record.retry_after_unix_seconds.is_some()
            {
                return Err(RecordError::Corrupt(
                    "Current lacks reconciliation completion",
                ));
            }
        }
        DerivationPhase::GraphCommittedAwaitingReconciliation => {
            let witness = record
                .witness
                .as_ref()
                .ok_or(RecordError::Corrupt("committed phase has no witness"))?;
            if record.pending_generation != Some(witness.graph_generation)
                || witness.search_reconciled
                || record.completed_generation.is_some()
            {
                return Err(RecordError::Corrupt(
                    "pending reconciliation witness is inconsistent",
                ));
            }
        }
        DerivationPhase::Blocked if record.last_error.is_none() => {
            return Err(RecordError::Corrupt("Blocked has no reason"));
        }
        DerivationPhase::Pending | DerivationPhase::Running
            if record.witness.is_some()
                || record.pending_generation.is_some()
                || record.completed_generation.is_some() =>
        {
            return Err(RecordError::Corrupt(
                "uncommitted phase retains a completion witness",
            ));
        }
        _ => {}
    }
    Ok(())
}

fn check_records(
    records: &DerivationRecords,
    identity: &PublicationIdentity,
) -> Result<(), RecordError> {
    if records.vaults.len() > MAX_VAULT_RECORDS {
        return Err(RecordError::LimitExceeded);
    }
    for (uid, record) in &records.vaults {
        if uid != &record.vault_uid {
            return Err(RecordError::Corrupt("record key differs from vault UID"));
        }
        check_record(record, identity)?;
    }
    Ok(())
}

/// Decode integrity and all identity bindings before classifying any record's
/// derivation version. A future version in one vault is handled at admission
/// for that vault; it does not silently become a missing entry.
pub fn decode_records(
    bytes: &[u8],
    expected: &RecordExpectation<'_>,
) -> Result<DerivationRecords, RecordError> {
    check_expectation(expected)?;
    if bytes.len() > MAX_RECORD_BYTES {
        return Err(RecordError::LimitExceeded);
    }
    let value: serde_json::Value =
        serde_json::from_slice(bytes).map_err(|_| RecordError::Corrupt("invalid JSON"))?;
    let schema = value
        .get("schema_version")
        .and_then(|v| v.as_u64())
        .ok_or(RecordError::Corrupt("schema is absent"))?;
    if schema != u64::from(RECORD_SCHEMA_VERSION) {
        return Err(RecordError::UnsupportedSchema(schema));
    }
    let envelope: RecordEnvelope = serde_json::from_value(value)
        .map_err(|_| RecordError::Corrupt("invalid envelope fields"))?;
    envelope
        .identity
        .validate()
        .map_err(|_| RecordError::InvalidIdentity)?;
    if &envelope.identity != expected.identity
        || envelope.data_instance_id != expected.data_instance_id
    {
        return Err(RecordError::ForeignIdentity);
    }
    let payload = serde_json::to_vec(&envelope.payload)
        .map_err(|_| RecordError::Corrupt("payload cannot be encoded"))?;
    if blake3::hash(&payload).to_hex().as_str() != envelope.payload_checksum {
        return Err(RecordError::Corrupt("payload checksum mismatch"));
    }
    let records: DerivationRecords = serde_json::from_value(envelope.payload)
        .map_err(|_| RecordError::Corrupt("invalid record payload"))?;
    check_records(&records, expected.identity)?;
    Ok(records)
}

pub fn load_records(
    db_path: &Path,
    expected: &RecordExpectation<'_>,
) -> Result<Option<DerivationRecords>, RecordError> {
    check_expectation(expected)?;
    let file = match std::fs::File::open(record_path(db_path)) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(RecordError::Io(error.kind())),
    };
    let mut bytes = Vec::new();
    file.take(MAX_RECORD_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| RecordError::Io(e.kind()))?;
    decode_records(&bytes, expected).map(Some)
}

fn bounded_json(value: &impl Serialize) -> Result<Vec<u8>, RecordError> {
    struct BoundedBytes {
        bytes: Vec<u8>,
        exceeded: bool,
    }
    impl Write for BoundedBytes {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            if bytes.len() > MAX_RECORD_BYTES.saturating_sub(self.bytes.len()) {
                self.exceeded = true;
                return Err(std::io::Error::other("record byte limit exceeded"));
            }
            self.bytes.extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut writer = BoundedBytes {
        bytes: Vec::new(),
        exceeded: false,
    };
    let result = serde_json::to_writer(&mut writer, value);
    if writer.exceeded {
        return Err(RecordError::LimitExceeded);
    }
    result.map_err(|_| RecordError::Corrupt("record encoding failed"))?;
    Ok(writer.bytes)
}

fn encode_records(
    records: &DerivationRecords,
    expected: &RecordExpectation<'_>,
) -> Result<Vec<u8>, RecordError> {
    check_expectation(expected)?;
    check_records(records, expected.identity)?;
    if let Some(record) = records
        .vaults
        .values()
        .find(|r| r.derivation_version > DERIVATION_VERSION)
    {
        return Err(RecordError::FutureVersion(record.derivation_version));
    }
    let payload_bytes = bounded_json(records)?;
    let payload: serde_json::Value = serde_json::from_slice(&payload_bytes)
        .map_err(|_| RecordError::Corrupt("record encoding failed"))?;
    // Hash the normalized JSON object order used by decode_records.
    let payload_bytes = bounded_json(&payload)?;
    let envelope = RecordEnvelope {
        schema_version: RECORD_SCHEMA_VERSION,
        identity: expected.identity.clone(),
        data_instance_id: expected.data_instance_id.to_owned(),
        payload_checksum: blake3::hash(&payload_bytes).to_hex().to_string(),
        payload,
    };
    bounded_json(&envelope)
}

/// Caller must hold the admitted daemon writer through this replacement.
/// Suspect or future incumbent bytes are preserved; this is not a repair or
/// administrative override API. Atomic persistence advances no graph epoch.
pub fn save_records(
    db_path: &Path,
    records: &DerivationRecords,
    expected: &RecordExpectation<'_>,
) -> Result<(), RecordError> {
    if let Some(old) = load_records(db_path, expected)?
        && let Some(record) = old
            .vaults
            .values()
            .find(|r| r.derivation_version > DERIVATION_VERSION)
    {
        return Err(RecordError::FutureVersion(record.derivation_version));
    }
    let bytes = encode_records(records, expected)?;
    nestweaver_store::durable_sidecar::atomic_replace_file(&record_path(db_path), |file| {
        file.write_all(&bytes)
    })
    .map_err(|e| RecordError::Io(e.kind()))
}

/// Admission for persistent graph state. Callers must separately establish
/// authorization, live publication readiness and genuine ephemeral-store
/// semantics; none is inferred from an absent record here.
pub fn admit_vault(
    records: Option<&DerivationRecords>,
    expected: &RecordExpectation<'_>,
    vault: &Vault,
    source: &SourceIdentity,
    coverage: &CoverageIdentity,
    read_only: bool,
) -> Result<(), DerivationUnavailable> {
    check_expectation(expected).map_err(|e| DerivationUnavailable::from_record_error(&e))?;
    let Some(record) = records.and_then(|r| r.vaults.get(&vault.uid)) else {
        return Err(DerivationUnavailable::pending(
            AdmissionReason::MissingRecord,
            read_only,
        ));
    };
    check_record(record, expected.identity)
        .map_err(|e| DerivationUnavailable::from_record_error(&e))?;
    if record.vault_uid != vault.uid {
        return Err(DerivationUnavailable::from_record_error(
            &RecordError::ForeignIdentity,
        ));
    }
    if record.derivation_version > DERIVATION_VERSION {
        return Err(DerivationUnavailable::from_record_error(
            &RecordError::FutureVersion(record.derivation_version),
        ));
    }
    if record.source != *source
        || record.coverage != *coverage
        || record.data_instance_id != vault.instance_id
    {
        return Err(DerivationUnavailable::pending(
            AdmissionReason::SourceChanged,
            read_only,
        ));
    }
    if record.derivation_version < DERIVATION_VERSION {
        return Err(DerivationUnavailable::pending(
            AdmissionReason::OlderVersion,
            read_only,
        ));
    }
    match record.phase {
        DerivationPhase::Current => Ok(()),
        DerivationPhase::GraphCommittedAwaitingReconciliation => Err(
            DerivationUnavailable::pending(AdmissionReason::ReconciliationPending, read_only),
        ),
        DerivationPhase::Pending | DerivationPhase::Running => Err(DerivationUnavailable::pending(
            AdmissionReason::MigrationPending,
            read_only,
        )),
        DerivationPhase::Blocked => Err(DerivationUnavailable {
            code: "vault_derivation_unavailable",
            reason: if read_only {
                AdmissionReason::UpstreamRequired
            } else {
                AdmissionReason::SourceBlocked
            },
            retryable: false,
            expected_version: DERIVATION_VERSION,
        }),
    }
}

impl VaultDerivationRecord {
    pub fn pending(vault: &Vault, source: SourceIdentity, coverage: CoverageIdentity) -> Self {
        Self {
            vault_uid: vault.uid.clone(),
            data_instance_id: vault.instance_id.clone(),
            source,
            coverage,
            derivation_version: DERIVATION_VERSION,
            phase: DerivationPhase::Pending,
            attempts: 0,
            last_error: None,
            retry_after_unix_seconds: None,
            pending_generation: None,
            completed_generation: None,
            witness: None,
        }
    }

    /// Retain actual full engine evidence, not a count-only success signal.
    /// The caller must also verify the live marker is clean while holding its
    /// writer and capture an inventory digest from the same authorized source.
    pub fn record_complete_graph(
        &mut self,
        result: &crate::index_md::MarkdownRefreshResult,
        expected_notes: usize,
        inventory_digest: String,
        identity: &PublicationIdentity,
    ) -> Result<(), RecordError> {
        check_record(self, identity)?;
        if self.derivation_version != DERIVATION_VERSION
            || result.index.vault_uid != self.vault_uid
            || result.index.notes_count != expected_notes
            || result.index.skipped.iter().any(is_coverage_gap)
            || result.publication.disposition
                != crate::manifest::GraphMutationPublicationDisposition::CommittedComplete
            || !result.publication.warnings.is_empty()
            || !digest_valid(&inventory_digest)
            || result.publication.generation_after <= result.publication.generation_before
        {
            return Err(RecordError::IncompletePublication);
        }
        let mut next = self.clone();
        next.phase = DerivationPhase::GraphCommittedAwaitingReconciliation;
        next.pending_generation = Some(result.publication.generation_after);
        next.completed_generation = None;
        next.witness = Some(CompletionWitness {
            publication_identity: identity.clone(),
            vault_uid: self.vault_uid.clone(),
            source: self.source.clone(),
            coverage: self.coverage.clone(),
            derivation_version: self.derivation_version,
            source_inventory_digest: inventory_digest,
            notes_count: expected_notes as u64,
            graph_generation: result.publication.generation_after,
            search_reconciled: false,
        });
        check_record(&next, identity)?;
        *self = next;
        Ok(())
    }

    /// Call only after the required search reconciliation succeeded (including
    /// intentional Disabled policy), under the same admitted writer. Persist
    /// the returned transition before publishing read-side readiness.
    pub fn record_search_reconciled(
        &mut self,
        identity: &PublicationIdentity,
    ) -> Result<(), RecordError> {
        check_record(self, identity)?;
        if self.phase != DerivationPhase::GraphCommittedAwaitingReconciliation {
            return Err(RecordError::IncompletePublication);
        }
        let mut next = self.clone();
        let witness = next
            .witness
            .as_mut()
            .ok_or(RecordError::IncompletePublication)?;
        witness.search_reconciled = true;
        next.completed_generation = Some(witness.graph_generation);
        next.pending_generation = None;
        next.phase = DerivationPhase::Current;
        next.last_error = None;
        next.retry_after_unix_seconds = None;
        check_record(&next, identity)?;
        *self = next;
        Ok(())
    }
}

/// Whether a skipped file leaves a vault's Markdown derivation incomplete.
///
/// `Ignored` (a `.brainignore` match or a default-pruned directory),
/// `Unsupported`, `Oversized` and `Binary` skips are decided by the coverage
/// policy the record already binds, so re-running derivation can never change
/// them. Counting them as gaps made every vault with an ignore rule impossible
/// to stamp. Read and parse failures, cancellation and unknown causes are real
/// gaps.
pub fn is_coverage_gap(skip: &nestweaver_parser::SkippedFile) -> bool {
    use nestweaver_parser::SkipReasonCode;
    !matches!(
        skip.reason_code,
        SkipReasonCode::Ignored
            | SkipReasonCode::Unsupported
            | SkipReasonCode::Oversized
            | SkipReasonCode::Binary
    )
}

/// Blocked admission is not retryable, so the writer retries a Blocked record
/// on its own backoff schedule instead. A Blocked record with no schedule is
/// due at once rather than stranded.
pub fn blocked_retry_due(record: &VaultDerivationRecord, now_unix_seconds: u64) -> bool {
    record.phase == DerivationPhase::Blocked
        && record
            .retry_after_unix_seconds
            .is_none_or(|retry_after| now_unix_seconds >= retry_after)
}

/// Tools whose answers depend on current Markdown link derivation.
pub fn requires_current_derivation(tool: &str) -> bool {
    matches!(
        tool,
        "brain_context"
            | "code_context"
            | "project_context"
            | "brain_impact"
            | "flow_trace"
            | "backlinks"
            | "brain_broken_links"
            | "brain_orphan_documents"
            | "brain_topic_clusters"
            | "brain_tag_graph"
            | "brain_doc_stats"
    )
}

pub fn filesystem_source(root: &Path) -> Result<SourceIdentity, RecordError> {
    let canonical = root.canonicalize().map_err(|e| RecordError::Io(e.kind()))?;
    Ok(SourceIdentity {
        provider: SourceProvider::Filesystem,
        canonical_root: canonical.to_string_lossy().into_owned(),
        provider_repo_uid: None,
    })
}

pub fn coverage_identity(
    root: &Path,
    extra_ignore_patterns: &[String],
    max_note_bytes: u64,
    scope: CoverageScope,
) -> Result<CoverageIdentity, RecordError> {
    let mut extra: Vec<String> = extra_ignore_patterns
        .iter()
        .map(|pattern| pattern.trim().to_owned())
        .filter(|pattern| !pattern.is_empty())
        .collect();
    extra.sort();
    extra.dedup();
    let ignore_path = root.join(".brainignore");
    let brainignore = match std::fs::read(&ignore_path) {
        Ok(bytes) if bytes.len() <= 64 * 1024 => blake3::hash(&bytes).to_hex().to_string(),
        Ok(_) => return Err(RecordError::LimitExceeded),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => "absent".to_owned(),
        Err(error) => {
            return Err(RecordError::IgnorePolicyUnreadable {
                path: ignore_path,
                kind: error.kind(),
            });
        }
    };
    let payload = serde_json::json!({
        "extra_ignore_patterns": extra,
        "max_note_bytes": max_note_bytes,
        "brainignore": brainignore,
    });
    let encoded = serde_json::to_vec(&payload)
        .map_err(|_| RecordError::Corrupt("coverage encoding failed"))?;
    Ok(CoverageIdentity {
        scope,
        policy_digest: blake3::hash(&encoded).to_hex().to_string(),
        max_note_bytes,
        extra_ignore_patterns: extra,
    })
}

pub fn inventory_digest(notes: &[nestweaver_schema::Note]) -> String {
    let mut rows: Vec<(&str, &str, &str)> = notes
        .iter()
        .map(|note| {
            (
                note.file_path.as_str(),
                note.content_hash.as_str(),
                note.uid.as_str(),
            )
        })
        .collect();
    rows.sort_unstable();
    blake3::hash(&serde_json::to_vec(&rows).unwrap_or_default())
        .to_hex()
        .to_string()
}

pub fn expectation<'a>(
    identity: &'a PublicationIdentity,
    data_instance_id: &'a str,
) -> RecordExpectation<'a> {
    RecordExpectation {
        identity,
        data_instance_id,
    }
}

/// Admit every persistent vault for note-edge-dependent reads.
pub fn admit_all_vaults(
    store: &nestweaver_store::GraphStore,
    db_path: &Path,
    data_instance_id: &str,
    extra_ignore_patterns: &[String],
    max_note_bytes: u64,
    read_only: bool,
) -> Result<(), DerivationUnavailable> {
    let vaults = store.list_vaults(None).map_err(|_| {
        DerivationUnavailable::from_record_error(&RecordError::Io(std::io::ErrorKind::Other))
    })?;
    if vaults.is_empty() {
        return Ok(());
    }
    let identity =
        store.publication_identity().ok().flatten().ok_or_else(|| {
            DerivationUnavailable::from_record_error(&RecordError::InvalidIdentity)
        })?;
    let expected = expectation(&identity, data_instance_id);
    let records = load_records(db_path, &expected)?;
    for vault in &vaults {
        let source = filesystem_source(Path::new(&vault.root_path))
            .map_err(|e| DerivationUnavailable::from_record_error(&e))?;
        let mut coverage = coverage_identity(
            Path::new(&source.canonical_root),
            extra_ignore_patterns,
            max_note_bytes,
            CoverageScope::FullRegisteredPolicy,
        )
        .map_err(|e| DerivationUnavailable::from_record_error(&e))?;
        if let Some(recorded) = records
            .as_ref()
            .and_then(|records| records.vaults.get(&vault.uid))
        {
            coverage.scope = recorded.coverage.scope.clone();
        }
        admit_vault(
            records.as_ref(),
            &expected,
            vault,
            &source,
            &coverage,
            read_only,
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use nestweaver_schema::vault_uid;

    fn identity() -> PublicationIdentity {
        PublicationIdentity::new_brain()
    }

    /// nw-684 review: the derivation gate reads `.brainignore` for its policy
    /// digest; an unreadable one must name the file and the remedy, not
    /// report an anonymous "record I/O failed (PermissionDenied)". The
    /// admission code stays the path-free `RecordUnavailable`.
    #[cfg(unix)]
    #[test]
    fn unreadable_brainignore_in_coverage_identity_names_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join(".brainignore");
        std::fs::write(&file, "secret.md\n").unwrap();
        let Some(_restore) = crate::brainignore::test_support::make_unreadable(&file) else {
            return;
        };
        let error = coverage_identity(dir.path(), &[], 1024, CoverageScope::FullRegisteredPolicy)
            .expect_err("unreadable policy");
        let message = error.to_string();
        assert!(
            message.contains(&file.display().to_string()) && message.contains("readable"),
            "{message}"
        );
        let admission = DerivationUnavailable::from_record_error(&error);
        assert_eq!(admission.reason, AdmissionReason::RecordUnavailable);
        assert!(!admission.to_string().contains(".brainignore"));
    }

    #[test]
    fn structural_tools_are_gated_and_note_status_are_not() {
        assert!(requires_current_derivation("brain_context"));
        assert!(requires_current_derivation("backlinks"));
        assert!(requires_current_derivation("brain_broken_links"));
        assert!(!requires_current_derivation("note_get"));
        assert!(!requires_current_derivation("brain_status"));
        assert!(!requires_current_derivation("brain_search"));
    }

    #[test]
    fn missing_record_is_pending_not_current() {
        let id = identity();
        let expected = expectation(&id, "brain");
        let vault = Vault {
            uid: vault_uid("brain", "/tmp/vault"),
            name: "vault".into(),
            root_path: "/tmp/vault".into(),
            instance_id: "brain".into(),
        };
        let source = SourceIdentity {
            provider: SourceProvider::Filesystem,
            canonical_root: "/tmp/vault".into(),
            provider_repo_uid: None,
        };
        let coverage = CoverageIdentity {
            scope: CoverageScope::LegacyIndexedInventory,
            policy_digest: "a".repeat(64),
            max_note_bytes: 1024,
            extra_ignore_patterns: Vec::new(),
        };
        let error = admit_vault(None, &expected, &vault, &source, &coverage, false).unwrap_err();
        assert_eq!(error.reason, AdmissionReason::MissingRecord);
        assert!(error.retryable);
    }

    #[test]
    fn admit_reuses_recorded_scope_instead_of_treating_it_as_source_change() {
        let id = identity();
        let expected = expectation(&id, "brain");
        let vault = Vault {
            uid: vault_uid("brain", "/tmp/vault"),
            name: "vault".into(),
            root_path: "/tmp/vault".into(),
            instance_id: "brain".into(),
        };
        let source = SourceIdentity {
            provider: SourceProvider::Filesystem,
            canonical_root: "/tmp/vault".into(),
            provider_repo_uid: None,
        };
        let legacy = CoverageIdentity {
            scope: CoverageScope::LegacyIndexedInventory,
            policy_digest: "a".repeat(64),
            max_note_bytes: 1024,
            extra_ignore_patterns: Vec::new(),
        };
        let mut full = legacy.clone();
        full.scope = CoverageScope::FullRegisteredPolicy;
        let records = DerivationRecords {
            vaults: BTreeMap::from([(
                vault.uid.clone(),
                VaultDerivationRecord::pending(&vault, source.clone(), legacy.clone()),
            )]),
        };
        let mismatched =
            admit_vault(Some(&records), &expected, &vault, &source, &full, false).unwrap_err();
        assert_eq!(mismatched.reason, AdmissionReason::SourceChanged);
        let reused =
            admit_vault(Some(&records), &expected, &vault, &source, &legacy, false).unwrap_err();
        assert_eq!(reused.reason, AdmissionReason::MigrationPending);
    }

    #[test]
    fn only_failure_skips_are_coverage_gaps() {
        use nestweaver_parser::{SkipReasonCode, SkippedFile};
        for code in [
            SkipReasonCode::Ignored,
            SkipReasonCode::Unsupported,
            SkipReasonCode::Oversized,
            SkipReasonCode::Binary,
        ] {
            assert!(
                !is_coverage_gap(&SkippedFile::new("a.md", code, "policy")),
                "{code:?} is decided by coverage policy; retrying cannot change it"
            );
        }
        for code in [
            SkipReasonCode::ReadError,
            SkipReasonCode::ParseError,
            SkipReasonCode::Cancelled,
            SkipReasonCode::Other,
        ] {
            assert!(
                is_coverage_gap(&SkippedFile::new("a.md", code, "failure")),
                "{code:?} is a failure and must keep derivation incomplete"
            );
        }
    }

    #[test]
    fn complete_graph_accepts_policy_skips_and_refuses_failure_skips() {
        use crate::index_md::{MarkdownIndexResult, MarkdownRefreshResult};
        use crate::manifest::{
            GraphMutationPublicationDisposition, GraphMutationPublicationOutcome,
        };
        use nestweaver_parser::{SkipReasonCode, SkippedFile};
        let id = identity();
        let vault = Vault {
            uid: vault_uid("brain", "/tmp/vault"),
            name: "vault".into(),
            root_path: "/tmp/vault".into(),
            instance_id: "brain".into(),
        };
        let source = SourceIdentity {
            provider: SourceProvider::Filesystem,
            canonical_root: "/tmp/vault".into(),
            provider_repo_uid: None,
        };
        let coverage = CoverageIdentity {
            scope: CoverageScope::FullRegisteredPolicy,
            policy_digest: "a".repeat(64),
            max_note_bytes: 1024,
            extra_ignore_patterns: Vec::new(),
        };
        let result = |skipped: Vec<SkippedFile>| MarkdownRefreshResult {
            index: MarkdownIndexResult {
                vault_uid: vault.uid.clone(),
                vault_name: "vault".into(),
                notes_count: 2,
                headings_count: 2,
                sections_count: 2,
                tags_count: 0,
                resolved_link_edges: 1,
                unresolved_link_occurrences: 0,
                unresolved_link_section_targets: 0,
                unresolved_link_targets: 0,
                skipped,
                frontmatter_unparsed: Vec::new(),
            },
            notes_deleted: 0,
            publication: GraphMutationPublicationOutcome {
                disposition: GraphMutationPublicationDisposition::CommittedComplete,
                generation_before: 1,
                generation_after: 2,
                warnings: Vec::new(),
            },
            notes_near_size_limit: Vec::new(),
        };

        let mut record = VaultDerivationRecord::pending(&vault, source.clone(), coverage.clone());
        record
            .record_complete_graph(
                &result(vec![SkippedFile::new(
                    "secret.md",
                    SkipReasonCode::Ignored,
                    "matched .brainignore pattern",
                )]),
                2,
                "b".repeat(64),
                &id,
            )
            .unwrap();
        assert_eq!(
            record.phase,
            DerivationPhase::GraphCommittedAwaitingReconciliation
        );
        assert_eq!(record.pending_generation, Some(2));

        let mut record = VaultDerivationRecord::pending(&vault, source, coverage);
        let refused = record.record_complete_graph(
            &result(vec![SkippedFile::new(
                "B.md",
                SkipReasonCode::ReadError,
                "read error",
            )]),
            2,
            "b".repeat(64),
            &id,
        );
        assert_eq!(refused, Err(RecordError::IncompletePublication));
        assert_eq!(record.phase, DerivationPhase::Pending);
    }

    #[test]
    fn blocked_record_is_retry_due_only_after_its_backoff() {
        let vault = Vault {
            uid: vault_uid("brain", "/tmp/vault"),
            name: "vault".into(),
            root_path: "/tmp/vault".into(),
            instance_id: "brain".into(),
        };
        let source = SourceIdentity {
            provider: SourceProvider::Filesystem,
            canonical_root: "/tmp/vault".into(),
            provider_repo_uid: None,
        };
        let coverage = CoverageIdentity {
            scope: CoverageScope::LegacyIndexedInventory,
            policy_digest: "a".repeat(64),
            max_note_bytes: 1024,
            extra_ignore_patterns: Vec::new(),
        };
        let mut record = VaultDerivationRecord::pending(&vault, source, coverage);
        assert!(
            !blocked_retry_due(&record, 1_000),
            "a pending record is migrated through admission, not the blocked retry"
        );
        record.phase = DerivationPhase::Blocked;
        record.last_error = Some(BlockedReason::SourceUnavailable);
        record.retry_after_unix_seconds = Some(1_000);
        assert!(!blocked_retry_due(&record, 999));
        assert!(blocked_retry_due(&record, 1_000));
        record.retry_after_unix_seconds = None;
        assert!(
            blocked_retry_due(&record, 0),
            "a blocked record with no schedule must not be stranded"
        );
    }
}
