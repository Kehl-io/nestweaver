//! Durable, optimistic-CAS state for cancellable publication rebuilds.
//!
//! The operation journal lives outside immutable slots. Every checkpoint is a
//! checksummed temp/fsync/rename/parent-fsync replacement, and every writer
//! supplies the revision it observed so a resumed or duplicated process cannot
//! overwrite newer progress.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};

pub const OPERATION_STATE_VERSION: u32 = 1;
pub const OPERATION_STATE_FILE: &str = "state.json";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PublicationPhase {
    Planned,
    Graph,
    TextSearch,
    Regex,
    Embeddings,
    Metadata,
    Validating,
    Ready,
    Activating,
    Activated,
    Cancelled,
}

impl PublicationPhase {
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Activated | Self::Cancelled)
    }

    fn successor(self) -> Option<Self> {
        match self {
            Self::Planned => Some(Self::Graph),
            Self::Graph => Some(Self::TextSearch),
            Self::TextSearch => Some(Self::Regex),
            Self::Regex => Some(Self::Embeddings),
            Self::Embeddings => Some(Self::Metadata),
            Self::Metadata => Some(Self::Validating),
            Self::Validating => Some(Self::Ready),
            Self::Ready => Some(Self::Activating),
            Self::Activating => Some(Self::Activated),
            Self::Activated | Self::Cancelled => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PublicationOperationPlan {
    pub operation_uuid: String,
    pub brain_uuid: String,
    pub target_publication_uuid: String,
    pub expected_current_publication_uuid: Option<String>,
    pub input_fingerprint: String,
    pub producer_version: String,
    pub publication_format_version: u32,
    pub created_unix_millis: u64,
}

impl PublicationOperationPlan {
    pub fn validate(&self) -> anyhow::Result<()> {
        parse_non_nil_uuid("operation_uuid", &self.operation_uuid)?;
        parse_non_nil_uuid("brain_uuid", &self.brain_uuid)?;
        let target = parse_non_nil_uuid("target_publication_uuid", &self.target_publication_uuid)?;
        if let Some(expected) = &self.expected_current_publication_uuid {
            let expected = parse_non_nil_uuid("expected_current_publication_uuid", expected)?;
            if expected == target {
                anyhow::bail!("target publication must differ from the expected incumbent");
            }
        }
        if self.input_fingerprint.is_empty()
            || self.producer_version.is_empty()
            || self.publication_format_version == 0
        {
            anyhow::bail!(
                "publication plan requires input fingerprint, producer version, and non-zero format version"
            );
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PublicationProgress {
    pub completed: u64,
    pub total: Option<u64>,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PublicationFailure {
    pub phase: PublicationPhase,
    pub code: String,
    pub message: String,
    pub retryable: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PublicationOperationState {
    pub state_version: u32,
    pub revision: u64,
    pub plan: PublicationOperationPlan,
    pub phase: PublicationPhase,
    pub cancel_requested: bool,
    pub progress: Option<PublicationProgress>,
    pub completed_artifacts: BTreeMap<String, String>,
    pub validated_manifest_blake3: Option<String>,
    pub failure: Option<PublicationFailure>,
    pub updated_unix_millis: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InvalidPublicationOperation {
    pub operation_uuid: String,
    pub error: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct PublicationOperationList {
    pub operations: Vec<PublicationOperationState>,
    pub invalid_operations: Vec<InvalidPublicationOperation>,
}

impl PublicationOperationList {
    pub fn is_empty(&self) -> bool {
        self.operations.is_empty() && self.invalid_operations.is_empty()
    }
}

impl PublicationOperationState {
    fn new(plan: PublicationOperationPlan) -> anyhow::Result<Self> {
        plan.validate()?;
        Ok(Self {
            state_version: OPERATION_STATE_VERSION,
            revision: 0,
            updated_unix_millis: plan.created_unix_millis,
            plan,
            phase: PublicationPhase::Planned,
            cancel_requested: false,
            progress: None,
            completed_artifacts: BTreeMap::new(),
            validated_manifest_blake3: None,
            failure: None,
        })
    }

    pub fn resumable_with(&self, requested: &PublicationOperationPlan) -> anyhow::Result<()> {
        requested.validate()?;
        if self.phase.is_terminal() {
            anyhow::bail!(
                "publication operation is terminal in phase {:?}",
                self.phase
            );
        }
        if self.plan.operation_uuid != requested.operation_uuid
            || self.plan.brain_uuid != requested.brain_uuid
            || self.plan.target_publication_uuid != requested.target_publication_uuid
            || self.plan.expected_current_publication_uuid
                != requested.expected_current_publication_uuid
            || self.plan.input_fingerprint != requested.input_fingerprint
            || self.plan.producer_version != requested.producer_version
            || self.plan.publication_format_version != requested.publication_format_version
        {
            anyhow::bail!(
                "publication operation is incompatible with the requested resume; discard it explicitly or resume with the original inputs"
            );
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct ChecksummedOperationState {
    checksum_blake3: String,
    state: PublicationOperationState,
}

pub fn operation_state_path(
    publication_root: &Path,
    operation_uuid: &str,
) -> anyhow::Result<PathBuf> {
    Ok(
        crate::publication::operation_path(publication_root, operation_uuid)?
            .join(OPERATION_STATE_FILE),
    )
}

pub fn create_operation(
    publication_root: &Path,
    plan: PublicationOperationPlan,
) -> anyhow::Result<PublicationOperationState> {
    let state = PublicationOperationState::new(plan)?;
    let operations = publication_root.join("operations");
    std::fs::create_dir_all(&operations)?;
    let operation_dir =
        crate::publication::operation_path(publication_root, &state.plan.operation_uuid)?;
    std::fs::create_dir(&operation_dir).map_err(|error| {
        anyhow::anyhow!(
            "create publication operation {}: {error}",
            operation_dir.display()
        )
    })?;
    nestweaver_store::durable_sidecar::sync_parent_directory_durable(&operation_dir)?;
    if let Err(error) = persist_state(&operation_dir.join(OPERATION_STATE_FILE), &state) {
        let _ = std::fs::remove_dir(&operation_dir);
        return Err(error);
    }
    Ok(state)
}

pub fn load_operation(
    publication_root: &Path,
    operation_uuid: &str,
) -> anyhow::Result<PublicationOperationState> {
    parse_non_nil_uuid("operation_uuid", operation_uuid)?;
    let path = operation_state_path(publication_root, operation_uuid)?;
    load_operation_from_path(&path, operation_uuid)
}

fn load_operation_from_path(
    path: &Path,
    operation_uuid: &str,
) -> anyhow::Result<PublicationOperationState> {
    let bytes = std::fs::read(path).map_err(|error| {
        anyhow::anyhow!("read publication operation {}: {error}", path.display())
    })?;
    let envelope: ChecksummedOperationState = serde_json::from_slice(&bytes).map_err(|error| {
        anyhow::anyhow!("decode publication operation {}: {error}", path.display())
    })?;
    validate_loaded_state(operation_uuid, &envelope)?;
    Ok(envelope.state)
}

pub fn list_operations(publication_root: &Path) -> anyhow::Result<PublicationOperationList> {
    let root = publication_root.join("operations");
    let entries = match std::fs::read_dir(&root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(PublicationOperationList::default());
        }
        Err(error) => return Err(error.into()),
    };
    let mut operations = Vec::new();
    let mut invalid_operations = Vec::new();
    for entry in entries {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if uuid::Uuid::parse_str(&name).is_err() {
            continue;
        }
        match load_operation(publication_root, &name) {
            Ok(state) => operations.push(state),
            Err(error) => invalid_operations.push(InvalidPublicationOperation {
                operation_uuid: name,
                error: format!("{error:#}"),
            }),
        }
    }
    operations.sort_by(|left, right| {
        left.plan
            .created_unix_millis
            .cmp(&right.plan.created_unix_millis)
            .then_with(|| left.plan.operation_uuid.cmp(&right.plan.operation_uuid))
    });
    invalid_operations.sort_by(|left, right| left.operation_uuid.cmp(&right.operation_uuid));
    Ok(PublicationOperationList {
        operations,
        invalid_operations,
    })
}

fn checkpoint_operation(
    publication_root: &Path,
    operation_uuid: &str,
    expected_revision: u64,
    update: impl FnOnce(&mut PublicationOperationState) -> anyhow::Result<()>,
) -> anyhow::Result<PublicationOperationState> {
    let incumbent = load_operation(publication_root, operation_uuid)?;
    if incumbent.revision != expected_revision {
        anyhow::bail!(
            "stale publication-operation writer: expected revision {expected_revision}, current revision {}",
            incumbent.revision
        );
    }
    let mut next = incumbent.clone();
    update(&mut next)?;
    validate_update(&incumbent, &next)?;
    next.revision = incumbent
        .revision
        .checked_add(1)
        .ok_or_else(|| anyhow::anyhow!("publication operation revision exhausted"))?;
    next.updated_unix_millis = unix_millis().max(incumbent.updated_unix_millis);
    persist_state(
        &operation_state_path(publication_root, operation_uuid)?,
        &next,
    )?;
    Ok(next)
}

pub fn advance_phase(
    publication_root: &Path,
    operation_uuid: &str,
    expected_revision: u64,
    phase: PublicationPhase,
) -> anyhow::Result<PublicationOperationState> {
    checkpoint_operation(
        publication_root,
        operation_uuid,
        expected_revision,
        |state| {
            if state.cancel_requested {
                anyhow::bail!("publication cancellation is requested");
            }
            if let Some(failure) = &state.failure {
                anyhow::bail!(
                    "publication is failed at {:?}: {}",
                    failure.phase,
                    failure.message
                );
            }
            if state.phase.successor() != Some(phase) {
                anyhow::bail!(
                    "invalid publication phase transition {:?} -> {phase:?}",
                    state.phase
                );
            }
            state.phase = phase;
            state.progress = None;
            Ok(())
        },
    )
}

pub fn update_progress(
    publication_root: &Path,
    operation_uuid: &str,
    expected_revision: u64,
    progress: PublicationProgress,
) -> anyhow::Result<PublicationOperationState> {
    checkpoint_operation(
        publication_root,
        operation_uuid,
        expected_revision,
        |state| {
            if state.phase.is_terminal() || state.cancel_requested || state.failure.is_some() {
                anyhow::bail!("publication operation is not accepting progress");
            }
            if progress.message.is_empty()
                || progress
                    .total
                    .is_some_and(|total| progress.completed > total)
            {
                anyhow::bail!("invalid publication progress");
            }
            state.progress = Some(progress);
            Ok(())
        },
    )
}

pub fn record_artifact(
    publication_root: &Path,
    operation_uuid: &str,
    expected_revision: u64,
    relative_path: String,
    blake3: String,
) -> anyhow::Result<PublicationOperationState> {
    checkpoint_operation(
        publication_root,
        operation_uuid,
        expected_revision,
        |state| {
            validate_relative_path(&relative_path)?;
            validate_digest(&blake3)?;
            if state.phase.is_terminal() || state.cancel_requested || state.failure.is_some() {
                anyhow::bail!("publication operation is not accepting artifacts");
            }
            if let Some(incumbent) = state.completed_artifacts.get(&relative_path)
                && incumbent != &blake3
            {
                anyhow::bail!(
                    "artifact {relative_path} was already checkpointed with a different digest"
                );
            }
            state.completed_artifacts.insert(relative_path, blake3);
            Ok(())
        },
    )
}

pub fn mark_ready(
    publication_root: &Path,
    operation_uuid: &str,
    expected_revision: u64,
) -> anyhow::Result<PublicationOperationState> {
    let incumbent = load_operation(publication_root, operation_uuid)?;
    if incumbent.revision != expected_revision {
        anyhow::bail!(
            "stale publication-operation writer: expected revision {expected_revision}, current revision {}",
            incumbent.revision
        );
    }
    if incumbent.phase != PublicationPhase::Validating {
        anyhow::bail!("publication can become ready only from validating");
    }
    let digest = validate_target_slot(publication_root, &incumbent)?;
    checkpoint_operation(
        publication_root,
        operation_uuid,
        expected_revision,
        |state| {
            state.validated_manifest_blake3 = Some(digest);
            state.phase = PublicationPhase::Ready;
            state.progress = None;
            Ok(())
        },
    )
}

/// Activate a validated slot under the incumbent graph publication lease.
/// Re-entering after a crash is idempotent: if `CURRENT` already selects the
/// target while the journal still says `activating`, only the final journal
/// checkpoint is performed.
pub fn activate_operation(
    publication_root: &Path,
    operation_uuid: &str,
    expected_revision: u64,
    lease: &nestweaver_store::IndexPublicationLease<'_>,
) -> anyhow::Result<PublicationOperationState> {
    let state = select_operation(publication_root, operation_uuid, expected_revision, lease)?;
    complete_activation(publication_root, operation_uuid, state.revision)
}

/// Select a validated publication but leave the journal in `Activating` so
/// the caller can run a startup/read smoke against the exact path that
/// `CURRENT` resolves. A failed smoke may roll the pointer back before the
/// A publication failure that retrying cannot fix.
///
/// nw-148: the rebuild path recorded a hardcoded `retryable = true` for every
/// failure, including ones that can never succeed on retry — a lost CURRENT
/// compare-and-swap (the expected predecessor will never be CURRENT again) and
/// a slot artifact that fails digest validation (a retry revalidates the same
/// corrupt bytes and fails identically). Because the flag drives
/// `resume_operation`, which refuses non-retryable failures, marking everything
/// retryable removed the only signal that would tell an operator to discard and
/// start fresh.
///
/// A marker type rather than string matching on the message, so the raise site
/// owns the classification and it cannot drift when wording changes.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct PermanentPublicationFailure(pub String);

impl PermanentPublicationFailure {
    /// True when `error` (or any of its sources) is permanent.
    pub fn is_permanent(error: &anyhow::Error) -> bool {
        error
            .chain()
            .any(|cause| cause.is::<PermanentPublicationFailure>())
    }
}

/// operation is made terminal.
pub fn select_operation(
    publication_root: &Path,
    operation_uuid: &str,
    expected_revision: u64,
    lease: &nestweaver_store::IndexPublicationLease<'_>,
) -> anyhow::Result<PublicationOperationState> {
    let mut state = load_operation(publication_root, operation_uuid)?;
    if state.revision != expected_revision {
        anyhow::bail!(
            "stale publication-operation writer: expected revision {expected_revision}, current revision {}",
            state.revision
        );
    }
    if state.failure.is_some() || state.cancel_requested {
        anyhow::bail!("publication operation is failed or cancelled");
    }
    if state.phase == PublicationPhase::Ready {
        state = advance_phase(
            publication_root,
            operation_uuid,
            state.revision,
            PublicationPhase::Activating,
        )?;
    } else if state.phase != PublicationPhase::Activating {
        anyhow::bail!("publication activation requires ready or activating phase");
    }

    let current = crate::publication::read_current(publication_root)?;
    let already_selected = current.as_ref().is_some_and(|pointer| {
        uuid_equal(
            &pointer.publication_uuid,
            &state.plan.target_publication_uuid,
        )
        .unwrap_or(false)
            && pointer.manifest_blake3
                == state.validated_manifest_blake3.clone().unwrap_or_default()
    });
    if !already_selected {
        let digest = validate_target_slot(publication_root, &state)?;
        if state.validated_manifest_blake3.as_deref() != Some(digest.as_str()) {
            anyhow::bail!("validated publication manifest digest changed before activation");
        }
        let identity = nestweaver_store::PublicationIdentity {
            brain_uuid: state.plan.brain_uuid.clone(),
            publication_uuid: state.plan.target_publication_uuid.clone(),
        };
        let pointer = crate::publication::CurrentPublicationPointer::new(
            &identity,
            state.plan.expected_current_publication_uuid.clone(),
            digest,
        )?;
        crate::publication::compare_and_swap_current(
            publication_root,
            lease,
            state.plan.expected_current_publication_uuid.as_deref(),
            &pointer,
        )?;
    }
    Ok(state)
}

pub fn complete_activation(
    publication_root: &Path,
    operation_uuid: &str,
    expected_revision: u64,
) -> anyhow::Result<PublicationOperationState> {
    let state = load_operation(publication_root, operation_uuid)?;
    if state.revision != expected_revision || state.phase != PublicationPhase::Activating {
        anyhow::bail!("publication completion requires the observed activating revision");
    }
    let current = crate::publication::read_current(publication_root)?
        .ok_or_else(|| anyhow::anyhow!("CURRENT disappeared before activation completed"))?;
    if !uuid_equal(
        &current.publication_uuid,
        &state.plan.target_publication_uuid,
    )? || current.manifest_blake3 != state.validated_manifest_blake3.clone().unwrap_or_default()
    {
        anyhow::bail!("CURRENT does not select the validated target publication");
    }
    advance_phase(
        publication_root,
        operation_uuid,
        state.revision,
        PublicationPhase::Activated,
    )
}

pub fn request_cancel(
    publication_root: &Path,
    operation_uuid: &str,
    expected_revision: u64,
) -> anyhow::Result<PublicationOperationState> {
    checkpoint_operation(
        publication_root,
        operation_uuid,
        expected_revision,
        |state| {
            if state.phase.is_terminal() {
                anyhow::bail!("cannot cancel terminal publication phase {:?}", state.phase);
            }
            state.cancel_requested = true;
            Ok(())
        },
    )
}

pub fn acknowledge_cancel(
    publication_root: &Path,
    operation_uuid: &str,
    expected_revision: u64,
) -> anyhow::Result<PublicationOperationState> {
    checkpoint_operation(
        publication_root,
        operation_uuid,
        expected_revision,
        |state| {
            if !state.cancel_requested {
                anyhow::bail!("publication cancellation was not requested");
            }
            state.phase = PublicationPhase::Cancelled;
            state.progress = None;
            Ok(())
        },
    )
}

pub fn record_failure(
    publication_root: &Path,
    operation_uuid: &str,
    expected_revision: u64,
    code: impl Into<String>,
    message: impl Into<String>,
    retryable: bool,
) -> anyhow::Result<PublicationOperationState> {
    let code = code.into();
    let message = message.into();
    checkpoint_operation(
        publication_root,
        operation_uuid,
        expected_revision,
        |state| {
            if state.phase.is_terminal() {
                anyhow::bail!("cannot fail terminal publication phase {:?}", state.phase);
            }
            if code.is_empty() || message.is_empty() {
                anyhow::bail!("publication failure requires a code and message");
            }
            state.failure = Some(PublicationFailure {
                phase: state.phase,
                code,
                message,
                retryable,
            });
            Ok(())
        },
    )
}

pub fn resume_operation(
    publication_root: &Path,
    requested: &PublicationOperationPlan,
    expected_revision: u64,
) -> anyhow::Result<PublicationOperationState> {
    checkpoint_operation(
        publication_root,
        &requested.operation_uuid,
        expected_revision,
        |state| {
            state.resumable_with(requested)?;
            if state
                .failure
                .as_ref()
                .is_some_and(|failure| !failure.retryable)
            {
                anyhow::bail!("publication failure is not retryable; discard explicitly");
            }
            state.failure = None;
            state.cancel_requested = false;
            Ok(())
        },
    )
}

/// Explicitly discard a cancelled or failed staging operation. The target slot
/// is first moved under the operation directory, then the complete operation is
/// renamed out of the active UUID namespace before recursive deletion. A crash
/// at any point therefore leaves either an inspectable active operation or an
/// ignored `.discarded-*` tombstone, never a half-deleted selectable slot.
pub fn discard_operation(
    publication_root: &Path,
    operation_uuid: &str,
    expected_revision: u64,
) -> anyhow::Result<()> {
    let state = load_operation(publication_root, operation_uuid)?;
    if state.revision != expected_revision {
        anyhow::bail!(
            "stale publication-operation writer: expected revision {expected_revision}, current revision {}",
            state.revision
        );
    }
    if state.phase != PublicationPhase::Cancelled && state.failure.is_none() {
        // nw-146: name the escape. A cancellation that was REQUESTED but never
        // acknowledged (its worker crashed, was Ctrl-C'd, or ran
        // --no-activate) lands here, and the old message left the operator
        // with no next step while a full graph copy stayed on disk.
        if state.cancel_requested {
            anyhow::bail!(
                "publication operation {} has a cancellation requested but not acknowledged \
                 (phase {:?}) — no worker is left to acknowledge it. Re-run with --force to \
                 acknowledge and discard in one step.",
                state.plan.operation_uuid,
                state.phase
            );
        }
        anyhow::bail!(
            "only a cancelled or failed staging operation may be discarded (phase {:?}); \
             cancel it first with `nestweaver publication cancel`",
            state.phase
        );
    }
    if crate::publication::read_current(publication_root)?.is_some_and(|pointer| {
        uuid_equal(
            &pointer.publication_uuid,
            &state.plan.target_publication_uuid,
        )
        .unwrap_or(false)
    }) {
        anyhow::bail!("refusing to discard the currently selected publication");
    }

    let operation_dir = crate::publication::operation_path(publication_root, operation_uuid)?;
    let slot =
        crate::publication::slot_path(publication_root, &state.plan.target_publication_uuid)?;
    if slot.exists() {
        let nested = operation_dir.join("discarded-slot");
        std::fs::rename(&slot, &nested)?;
        nestweaver_store::durable_sidecar::sync_parent_directory_durable(&slot)?;
        nestweaver_store::durable_sidecar::sync_parent_directory_durable(&nested)?;
    }
    let tombstone = publication_root.join("operations").join(format!(
        ".discarded-{}-{}",
        state.plan.operation_uuid, state.revision
    ));
    std::fs::rename(&operation_dir, &tombstone)?;
    nestweaver_store::durable_sidecar::sync_parent_directory_durable(&operation_dir)?;
    std::fs::remove_dir_all(&tombstone)?;
    nestweaver_store::durable_sidecar::sync_parent_directory_durable(&tombstone)?;
    Ok(())
}

/// Explicitly discard an unreadable operation journal without trusting any of
/// its contents. Publication slots are deliberately left untouched because an
/// invalid journal cannot safely identify its target or prove that target is
/// not selected.
pub fn discard_invalid_operation(
    publication_root: &Path,
    operation_uuid: &str,
) -> anyhow::Result<()> {
    parse_non_nil_uuid("operation_uuid", operation_uuid)?;
    let operation_dir = crate::publication::operation_path(publication_root, operation_uuid)?;
    let tombstone = publication_root.join("operations").join(format!(
        ".discarded-invalid-{operation_uuid}-{}",
        uuid::Uuid::new_v4()
    ));
    std::fs::rename(&operation_dir, &tombstone).map_err(|error| {
        anyhow::anyhow!(
            "quarantine invalid publication operation {}: {error}",
            operation_dir.display()
        )
    })?;
    nestweaver_store::durable_sidecar::sync_parent_directory_durable(&operation_dir)?;
    if load_operation_from_path(&tombstone.join(OPERATION_STATE_FILE), operation_uuid).is_ok() {
        std::fs::rename(&tombstone, &operation_dir).map_err(|error| {
            anyhow::anyhow!(
                "restore readable publication operation {} after invalid discard refusal: {error}",
                operation_dir.display()
            )
        })?;
        nestweaver_store::durable_sidecar::sync_parent_directory_durable(&operation_dir)?;
        anyhow::bail!(
            "publication operation journal is readable; discard it with an exact --revision \
             (add --force if a cancellation was requested but never acknowledged)"
        );
    }
    std::fs::remove_dir_all(&tombstone)?;
    nestweaver_store::durable_sidecar::sync_parent_directory_durable(&tombstone)?;
    Ok(())
}

fn validate_update(
    incumbent: &PublicationOperationState,
    next: &PublicationOperationState,
) -> anyhow::Result<()> {
    next.plan.validate()?;
    if next.state_version != OPERATION_STATE_VERSION
        || next.plan != incumbent.plan
        || next.revision != incumbent.revision
        || next.updated_unix_millis != incumbent.updated_unix_millis
    {
        anyhow::bail!("publication checkpoint attempted to rewrite immutable journal fields");
    }
    if incumbent.phase.is_terminal() && next != incumbent {
        anyhow::bail!("terminal publication operations are immutable");
    }
    if next.phase != incumbent.phase
        && incumbent.phase.successor() != Some(next.phase)
        && !(next.phase == PublicationPhase::Cancelled && next.cancel_requested)
    {
        anyhow::bail!(
            "invalid publication journal phase transition {:?} -> {:?}",
            incumbent.phase,
            next.phase
        );
    }
    Ok(())
}

fn validate_target_slot(
    publication_root: &Path,
    state: &PublicationOperationState,
) -> anyhow::Result<String> {
    let slot =
        crate::publication::slot_path(publication_root, &state.plan.target_publication_uuid)?;
    let path = slot.join(crate::publication::PUBLICATION_MANIFEST_FILE);
    let bytes = std::fs::read(&path)
        .map_err(|error| anyhow::anyhow!("read target manifest {}: {error}", path.display()))?;
    let bundle: crate::publication::PublicationBundleV3 = serde_json::from_slice(&bytes)?;
    bundle.validate_metadata(state.plan.publication_format_version)?;
    if !uuid_equal(&bundle.brain_uuid, &state.plan.brain_uuid)?
        || !uuid_equal(
            &bundle.publication_uuid,
            &state.plan.target_publication_uuid,
        )?
        || bundle.producer_version != state.plan.producer_version
    {
        anyhow::bail!("target publication manifest identity or producer does not match operation");
    }
    for descriptor in &bundle.artifacts {
        let artifact = slot.join(&descriptor.path);
        let (byte_size, digest) = crate::hash::blake3_file(&artifact).map_err(|error| {
            anyhow::anyhow!("stream target artifact {}: {error}", artifact.display())
        })?;
        if byte_size != descriptor.byte_size || digest != descriptor.blake3 {
            // Permanent: a retry revalidates the same bytes (nw-148).
            return Err(PermanentPublicationFailure(format!(
                "target artifact {} failed size or digest validation",
                descriptor.path
            ))
            .into());
        }
    }
    Ok(crate::hash::blake3_hex_bytes(&bytes))
}

fn validate_relative_path(path: &str) -> anyhow::Result<()> {
    let path = Path::new(path);
    if path.as_os_str().is_empty()
        || path.is_absolute()
        || path
            .components()
            .any(|component| !matches!(component, std::path::Component::Normal(_)))
    {
        anyhow::bail!("unsafe publication artifact path {}", path.display());
    }
    Ok(())
}

fn validate_digest(value: &str) -> anyhow::Result<()> {
    if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        anyhow::bail!("expected a 64-character hexadecimal BLAKE3 digest");
    }
    Ok(())
}

fn validate_loaded_state(
    requested_operation_uuid: &str,
    envelope: &ChecksummedOperationState,
) -> anyhow::Result<()> {
    if envelope.state.state_version != OPERATION_STATE_VERSION {
        anyhow::bail!(
            "publication operation state version {} is incompatible with supported version {}",
            envelope.state.state_version,
            OPERATION_STATE_VERSION
        );
    }
    envelope.state.plan.validate()?;
    if !uuid_equal(
        requested_operation_uuid,
        &envelope.state.plan.operation_uuid,
    )? {
        anyhow::bail!("publication operation path identity does not match journal identity");
    }
    let observed = state_checksum(&envelope.state)?;
    if observed != envelope.checksum_blake3 {
        anyhow::bail!(
            "publication operation checksum mismatch: recorded {}, observed {observed}",
            envelope.checksum_blake3
        );
    }
    Ok(())
}

fn persist_state(path: &Path, state: &PublicationOperationState) -> anyhow::Result<()> {
    let envelope = ChecksummedOperationState {
        checksum_blake3: state_checksum(state)?,
        state: state.clone(),
    };
    let bytes = serde_json::to_vec_pretty(&envelope)?;
    nestweaver_store::durable_sidecar::atomic_replace_file(path, |file| file.write_all(&bytes))?;
    Ok(())
}

fn state_checksum(state: &PublicationOperationState) -> anyhow::Result<String> {
    Ok(blake3::hash(&serde_json::to_vec(state)?)
        .to_hex()
        .to_string())
}

fn parse_non_nil_uuid(label: &str, value: &str) -> anyhow::Result<uuid::Uuid> {
    let parsed = uuid::Uuid::parse_str(value)
        .map_err(|error| anyhow::anyhow!("invalid {label} '{value}': {error}"))?;
    if parsed.is_nil() {
        anyhow::bail!("invalid {label}: nil UUID is not an identity");
    }
    Ok(parsed)
}

fn uuid_equal(left: &str, right: &str) -> anyhow::Result<bool> {
    Ok(uuid::Uuid::parse_str(left)? == uuid::Uuid::parse_str(right)?)
}

fn unix_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plan() -> PublicationOperationPlan {
        PublicationOperationPlan {
            operation_uuid: uuid::Uuid::new_v4().to_string(),
            brain_uuid: uuid::Uuid::new_v4().to_string(),
            target_publication_uuid: uuid::Uuid::new_v4().to_string(),
            expected_current_publication_uuid: Some(uuid::Uuid::new_v4().to_string()),
            input_fingerprint: "inputs-v1:abc".to_string(),
            producer_version: env!("CARGO_PKG_VERSION").to_string(),
            publication_format_version: crate::snapshot::SNAPSHOT_FORMAT_VERSION,
            created_unix_millis: 42,
        }
    }

    fn advance_to_validating(
        root: &Path,
        plan: &PublicationOperationPlan,
        mut state: PublicationOperationState,
    ) -> PublicationOperationState {
        for phase in [
            PublicationPhase::Graph,
            PublicationPhase::TextSearch,
            PublicationPhase::Regex,
            PublicationPhase::Embeddings,
            PublicationPhase::Metadata,
            PublicationPhase::Validating,
        ] {
            state = advance_phase(root, &plan.operation_uuid, state.revision, phase).unwrap();
        }
        state
    }

    fn write_target_bundle(root: &Path, plan: &PublicationOperationPlan) -> String {
        let slot = crate::publication::slot_path(root, &plan.target_publication_uuid).unwrap();
        std::fs::create_dir_all(&slot).unwrap();
        let bundle = crate::publication::PublicationBundleV3 {
            format_version: plan.publication_format_version,
            brain_uuid: plan.brain_uuid.clone(),
            publication_uuid: plan.target_publication_uuid.clone(),
            producer_version: plan.producer_version.clone(),
            source_graph_generation: 0,
            artifacts: Vec::new(),
        };
        let bytes = serde_json::to_vec_pretty(&bundle).unwrap();
        std::fs::write(
            slot.join(crate::publication::PUBLICATION_MANIFEST_FILE),
            &bytes,
        )
        .unwrap();
        crate::hash::blake3_hex_bytes(&bytes)
    }

    #[test]
    fn operation_checkpoints_are_durable_sequential_and_revision_cas_protected() {
        let dir = tempfile::tempdir().unwrap();
        let plan = plan();
        let created = create_operation(dir.path(), plan.clone()).unwrap();
        assert_eq!(
            load_operation(dir.path(), &plan.operation_uuid).unwrap(),
            created
        );

        let graph = advance_phase(
            dir.path(),
            &plan.operation_uuid,
            created.revision,
            PublicationPhase::Graph,
        )
        .unwrap();
        assert_eq!(graph.revision, 1);
        assert!(
            advance_phase(
                dir.path(),
                &plan.operation_uuid,
                created.revision,
                PublicationPhase::TextSearch,
            )
            .unwrap_err()
            .to_string()
            .contains("stale publication-operation writer")
        );
        assert!(
            advance_phase(
                dir.path(),
                &plan.operation_uuid,
                graph.revision,
                PublicationPhase::Embeddings,
            )
            .unwrap_err()
            .to_string()
            .contains("invalid publication phase transition")
        );
    }

    #[test]
    fn cancel_failure_and_resume_are_explicit_and_compatibility_checked() {
        let dir = tempfile::tempdir().unwrap();
        let plan = plan();
        let state = create_operation(dir.path(), plan.clone()).unwrap();
        let failed = record_failure(
            dir.path(),
            &plan.operation_uuid,
            state.revision,
            "network",
            "model fetch interrupted",
            true,
        )
        .unwrap();
        assert!(
            advance_phase(
                dir.path(),
                &plan.operation_uuid,
                failed.revision,
                PublicationPhase::Graph,
            )
            .is_err()
        );
        let mut incompatible = plan.clone();
        incompatible.input_fingerprint = "different".to_string();
        assert!(resume_operation(dir.path(), &incompatible, failed.revision).is_err());
        let resumed = resume_operation(dir.path(), &plan, failed.revision).unwrap();
        assert!(resumed.failure.is_none());
        let requested = request_cancel(dir.path(), &plan.operation_uuid, resumed.revision).unwrap();
        let cancelled =
            acknowledge_cancel(dir.path(), &plan.operation_uuid, requested.revision).unwrap();
        assert_eq!(cancelled.phase, PublicationPhase::Cancelled);
        assert!(resume_operation(dir.path(), &plan, cancelled.revision).is_err());
    }

    #[test]
    fn completed_graph_artifacts_survive_retryable_resume() {
        let dir = tempfile::tempdir().unwrap();
        let plan = plan();
        let created = create_operation(dir.path(), plan.clone()).unwrap();
        let graph = advance_phase(
            dir.path(),
            &plan.operation_uuid,
            created.revision,
            PublicationPhase::Graph,
        )
        .unwrap();
        let checkpoint = "graph/repo/0123456789abcdef.done".to_string();
        let digest = "a".repeat(64);
        let recorded = record_artifact(
            dir.path(),
            &plan.operation_uuid,
            graph.revision,
            checkpoint.clone(),
            digest.clone(),
        )
        .unwrap();
        assert_eq!(recorded.completed_artifacts.get(&checkpoint), Some(&digest));
        assert!(
            record_artifact(
                dir.path(),
                &plan.operation_uuid,
                recorded.revision,
                checkpoint.clone(),
                "b".repeat(64),
            )
            .unwrap_err()
            .to_string()
            .contains("different digest")
        );

        let failed = record_failure(
            dir.path(),
            &plan.operation_uuid,
            recorded.revision,
            "interrupted",
            "worker stopped after a repository checkpoint",
            true,
        )
        .unwrap();
        let resumed = resume_operation(dir.path(), &plan, failed.revision).unwrap();
        assert_eq!(resumed.completed_artifacts.get(&checkpoint), Some(&digest));
        assert_eq!(resumed.phase, PublicationPhase::Graph);
    }

    #[test]
    fn corrupt_or_path_mismatched_operation_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let plan = plan();
        create_operation(dir.path(), plan.clone()).unwrap();
        let path = operation_state_path(dir.path(), &plan.operation_uuid).unwrap();
        let mut envelope: ChecksummedOperationState =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        envelope.state.cancel_requested = true;
        std::fs::write(&path, serde_json::to_vec_pretty(&envelope).unwrap()).unwrap();
        assert!(
            load_operation(dir.path(), &plan.operation_uuid)
                .unwrap_err()
                .to_string()
                .contains("checksum mismatch")
        );
    }

    #[test]
    fn validated_operation_activates_once_and_recovers_after_pointer_only_crash() {
        let dir = tempfile::tempdir().unwrap();
        let incumbent_db = dir.path().join("incumbent.lbug");
        let incumbent = nestweaver_store::GraphStore::create(&incumbent_db).unwrap();
        let incumbent_identity = incumbent.publication_identity().unwrap().unwrap();
        let mut plan = plan();
        plan.brain_uuid = incumbent_identity.brain_uuid.clone();
        plan.expected_current_publication_uuid = None;
        let state = create_operation(dir.path(), plan.clone()).unwrap();
        let validating = advance_to_validating(dir.path(), &plan, state);
        let expected_digest = write_target_bundle(dir.path(), &plan);
        let ready = mark_ready(dir.path(), &plan.operation_uuid, validating.revision).unwrap();
        assert_eq!(
            ready.validated_manifest_blake3.as_deref(),
            Some(expected_digest.as_str())
        );
        let lease = incumbent.acquire_index_publication_lease().unwrap();
        let activated =
            activate_operation(dir.path(), &plan.operation_uuid, ready.revision, &lease).unwrap();
        assert_eq!(activated.phase, PublicationPhase::Activated);
        let current = crate::publication::read_current(dir.path())
            .unwrap()
            .unwrap();
        assert_eq!(current.publication_uuid, plan.target_publication_uuid);
        lease.release().unwrap();

        // A second operation simulates death after CURRENT commits but before
        // the final journal checkpoint. Re-entry must observe the selected
        // target and complete without attempting a conflicting CAS.
        let mut recovery_plan = plan.clone();
        recovery_plan.operation_uuid = uuid::Uuid::new_v4().to_string();
        recovery_plan.target_publication_uuid = uuid::Uuid::new_v4().to_string();
        recovery_plan.expected_current_publication_uuid = Some(current.publication_uuid);
        let state = create_operation(dir.path(), recovery_plan.clone()).unwrap();
        let validating = advance_to_validating(dir.path(), &recovery_plan, state);
        let digest = write_target_bundle(dir.path(), &recovery_plan);
        let ready = mark_ready(
            dir.path(),
            &recovery_plan.operation_uuid,
            validating.revision,
        )
        .unwrap();
        let activating = advance_phase(
            dir.path(),
            &recovery_plan.operation_uuid,
            ready.revision,
            PublicationPhase::Activating,
        )
        .unwrap();
        let target_identity = nestweaver_store::PublicationIdentity {
            brain_uuid: recovery_plan.brain_uuid.clone(),
            publication_uuid: recovery_plan.target_publication_uuid.clone(),
        };
        let pointer = crate::publication::CurrentPublicationPointer::new(
            &target_identity,
            recovery_plan.expected_current_publication_uuid.clone(),
            digest,
        )
        .unwrap();
        let lease = incumbent.acquire_index_publication_lease().unwrap();
        crate::publication::compare_and_swap_current(
            dir.path(),
            &lease,
            recovery_plan.expected_current_publication_uuid.as_deref(),
            &pointer,
        )
        .unwrap();
        let recovered = activate_operation(
            dir.path(),
            &recovery_plan.operation_uuid,
            activating.revision,
            &lease,
        )
        .unwrap();
        assert_eq!(recovered.phase, PublicationPhase::Activated);
        lease.release().unwrap();
    }

    #[test]
    fn list_and_explicit_discard_remove_only_unselected_staging() {
        let dir = tempfile::tempdir().unwrap();
        let plan = plan();
        let state = create_operation(dir.path(), plan.clone()).unwrap();
        assert_eq!(list_operations(dir.path()).unwrap().operations.len(), 1);
        let requested = request_cancel(dir.path(), &plan.operation_uuid, state.revision).unwrap();
        let cancelled =
            acknowledge_cancel(dir.path(), &plan.operation_uuid, requested.revision).unwrap();
        let slot =
            crate::publication::slot_path(dir.path(), &plan.target_publication_uuid).unwrap();
        std::fs::create_dir_all(&slot).unwrap();
        std::fs::write(slot.join("partial"), b"staging").unwrap();
        discard_operation(dir.path(), &plan.operation_uuid, cancelled.revision).unwrap();
        assert!(!slot.exists());
        assert!(list_operations(dir.path()).unwrap().is_empty());
    }

    #[test]
    fn corrupt_operation_does_not_hide_valid_operations_and_can_be_discarded_safely() {
        let dir = tempfile::tempdir().unwrap();
        let valid_plan = plan();
        create_operation(dir.path(), valid_plan.clone()).unwrap();
        let invalid_plan = plan();
        create_operation(dir.path(), invalid_plan.clone()).unwrap();
        std::fs::write(
            operation_state_path(dir.path(), &invalid_plan.operation_uuid).unwrap(),
            b"{not-json",
        )
        .unwrap();
        let invalid_slot =
            crate::publication::slot_path(dir.path(), &invalid_plan.target_publication_uuid)
                .unwrap();
        std::fs::create_dir_all(&invalid_slot).unwrap();
        std::fs::write(invalid_slot.join("unknown-staging"), b"preserve").unwrap();

        let listing = list_operations(dir.path()).unwrap();
        assert_eq!(listing.operations.len(), 1);
        assert_eq!(
            listing.operations[0].plan.operation_uuid,
            valid_plan.operation_uuid
        );
        assert_eq!(listing.invalid_operations.len(), 1);
        assert_eq!(
            listing.invalid_operations[0].operation_uuid,
            invalid_plan.operation_uuid
        );
        assert!(
            listing.invalid_operations[0]
                .error
                .contains("decode publication operation")
        );

        discard_invalid_operation(dir.path(), &invalid_plan.operation_uuid).unwrap();
        assert!(
            invalid_slot.exists(),
            "an unreadable journal cannot authorize deleting an unknown target slot"
        );
        let listing = list_operations(dir.path()).unwrap();
        assert_eq!(listing.operations.len(), 1);
        assert!(listing.invalid_operations.is_empty());
    }

    #[test]
    fn invalid_discard_refuses_a_readable_journal() {
        let dir = tempfile::tempdir().unwrap();
        let plan = plan();
        create_operation(dir.path(), plan.clone()).unwrap();
        let error = discard_invalid_operation(dir.path(), &plan.operation_uuid).unwrap_err();
        assert!(error.to_string().contains("journal is readable"));
    }

    /// nw-148: a hardcoded `retryable = true` marked permanently-failed
    /// operations as retryable, and the flag drives resume_operation — so the
    /// one signal that would tell an operator to discard and start fresh was
    /// never emitted.
    #[test]
    fn permanent_failures_are_distinguishable_from_retryable_ones() {
        let permanent: anyhow::Error =
            PermanentPublicationFailure("CURRENT compare-and-swap conflict: expected a, observed b".into())
                .into();
        assert!(PermanentPublicationFailure::is_permanent(&permanent));
        // The message must survive classification unchanged, since operators
        // and existing assertions read it.
        assert!(permanent.to_string().contains("compare-and-swap conflict"));

        // Wrapped in context, as the rebuild path propagates it.
        let wrapped = permanent.context("publication rebuild failed");
        assert!(
            PermanentPublicationFailure::is_permanent(&wrapped),
            "classification must survive context wrapping"
        );

        // An ordinary transient failure stays retryable.
        let transient = anyhow::anyhow!("connection reset while streaming artifact");
        assert!(!PermanentPublicationFailure::is_permanent(&transient));
    }

    /// nw-146: `request_cancel` only sets the flag; only the RUNNING worker
    /// acknowledges it. When that worker is gone, both discard paths refused
    /// and the staged slot — a full graph copy — was stranded with no CLI
    /// route to reclaim it.
    #[test]
    fn an_unacknowledged_cancel_is_discardable_and_says_so() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let state = create_operation(root, plan()).unwrap();

        // The worker is gone, so nothing ever acknowledges this.
        let cancelled =
            request_cancel(root, &state.plan.operation_uuid, state.revision).unwrap();
        assert!(cancelled.cancel_requested);
        assert_ne!(cancelled.phase, PublicationPhase::Cancelled);

        // Before: refused, and the message must now name the way out.
        let error =
            discard_operation(root, &cancelled.plan.operation_uuid, cancelled.revision)
                .expect_err("an unacknowledged cancel is not directly discardable");
        let message = error.to_string();
        assert!(
            message.contains("--force"),
            "the refusal must name the escape: {message}"
        );

        // --force path: acknowledge, then discard.
        let acknowledged =
            acknowledge_cancel(root, &cancelled.plan.operation_uuid, cancelled.revision).unwrap();
        assert_eq!(acknowledged.phase, PublicationPhase::Cancelled);
        discard_operation(root, &acknowledged.plan.operation_uuid, acknowledged.revision)
            .expect("an acknowledged cancellation must be discardable");
    }

    #[test]
    fn future_state_version_is_listed_as_invalid_and_remains_discardable() {
        let dir = tempfile::tempdir().unwrap();
        let plan = plan();
        let mut state = create_operation(dir.path(), plan.clone()).unwrap();
        state.state_version = OPERATION_STATE_VERSION + 1;
        persist_state(
            &operation_state_path(dir.path(), &plan.operation_uuid).unwrap(),
            &state,
        )
        .unwrap();

        let listing = list_operations(dir.path()).unwrap();
        assert!(listing.operations.is_empty());
        assert_eq!(listing.invalid_operations.len(), 1);
        assert!(
            listing.invalid_operations[0]
                .error
                .contains("state version 2 is incompatible")
        );

        discard_invalid_operation(dir.path(), &plan.operation_uuid).unwrap();
        assert!(list_operations(dir.path()).unwrap().is_empty());
    }
}
