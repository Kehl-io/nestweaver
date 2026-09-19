use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::content_reader::ContentReader;

#[path = "manifest_validation.rs"]
mod validation;

pub(crate) const MANIFEST_ARTIFACT_KIND: &str = "repo_manifest";
pub(crate) const MANIFEST_ARTIFACT_SCHEMA_VERSION: u32 = 2;
pub(crate) const MANIFEST_ALGORITHM_FINGERPRINT: &str = "nestweaver-repo-manifest-v2";

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ManifestInfo {
    pub package_name: Option<String>,
    pub dependencies: Vec<String>,
    /// Repo-relative file paths declared as package entry points by every
    /// `package.json` found in the repo at any depth (root or nested;
    /// `node_modules` and the other shared skip-dirs are excluded, and
    /// `.gitignore` is respected — see [`discover_package_json_entry_files`]).
    /// Extracted from `main`, `bin` (string or object), `exports` (a string,
    /// an array fallback list, or a nested condition/subpath map — every
    /// string target at any depth is collected), and `browser` only when its
    /// value is a string (the object form is a bundler replacement map, not
    /// an entry point). Each raw path is rebased onto the directory
    /// containing its own manifest and normalized to a repo-relative,
    /// `/`-joined path (`.`/`..` resolved); an entry is dropped when it is
    /// empty/whitespace-only, contains a `*` pattern, is absolute (`/...`),
    /// contains `\` or a `://` scheme, ends in `/` (a directory reference),
    /// or would escape the repo root — see
    /// [`rebase_package_json_entry`] for the exact rules. A non-JSON manifest
    /// format (e.g. CMake) may also contribute its own entries here. These
    /// are entry points for the package(s) and their symbols should not be
    /// flagged as dead code.
    #[serde(default)]
    pub entry_files: Vec<String>,
}

/// Parse the manifest file(s) found in `repo_path` and return extracted
/// package name and dependency list. The first recognized manifest format
/// wins for `package_name`/`dependencies`.
///
/// `entry_files` is independent of that first-format-wins choice: every
/// `package.json` in the repo, at any depth and including the root, is
/// discovered and unioned in (see [`discover_package_json_entry_files`]), so
/// a root manifest of a different format (e.g. `Cargo.toml`) does not hide a
/// nested `package.json`'s entry points, and a root `package.json` missing a
/// `name` field still contributes its entries even though it cannot win the
/// name/dependencies choice above.
pub fn parse_manifest(reader: &dyn ContentReader) -> ManifestInfo {
    let mut info = parse_package_json(reader)
        .or_else(|| parse_go_mod(reader))
        .or_else(|| parse_cargo_toml(reader))
        .or_else(|| parse_pyproject_toml(reader))
        .or_else(|| parse_requirements_txt(reader))
        .or_else(|| parse_composer_json(reader))
        .or_else(|| parse_gemfile(reader))
        .or_else(|| parse_pubspec_yaml(reader))
        .or_else(|| parse_package_swift(reader))
        .or_else(|| parse_csproj(reader))
        .or_else(|| parse_build_gradle_kts(reader))
        .or_else(|| parse_cmake(reader))
        .unwrap_or_default();

    for entry in discover_package_json_entry_files(reader) {
        if !info.entry_files.contains(&entry) {
            info.entry_files.push(entry);
        }
    }
    info
}

/// Persist a `HashMap<repo_uid, ManifestInfo>` as a JSON sidecar file.
pub fn save_manifest_cache(
    manifests: &HashMap<String, ManifestInfo>,
    path: &Path,
) -> Result<(), anyhow::Error> {
    let json = serde_json::to_string(manifests)?;
    atomic_replace_file(path, |file| file.write_all(json.as_bytes()))
}

/// Canonical manifest sidecar path for a database.
pub fn manifest_cache_path(db_path: &Path) -> PathBuf {
    crate::sidecar_path(db_path, ".manifests.json")
}

/// Load the canonical manifest sidecar, migrating the legacy replacement-
/// extension path when it is the only copy present.
pub fn load_manifest_cache_for_db(
    store: &nestweaver_store::GraphStore,
    db_path: &Path,
) -> Result<HashMap<String, ManifestInfo>, anyhow::Error> {
    Ok(crate::artifact_sidecar::load_json(
        store,
        &manifest_cache_path(db_path),
        MANIFEST_ARTIFACT_KIND,
        MANIFEST_ARTIFACT_SCHEMA_VERSION,
        MANIFEST_ALGORITHM_FINGERPRINT,
    )?
    .unwrap_or_default())
}

/// Availability of the manifest input snapshot, separate from index success.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ManifestUnavailableReason {
    Missing,
    StaleGeneration,
    ProducerChanged,
    PublicationInProgress,
    PendingSourceChange,
    IncompleteCoverage,
    SourceUnavailable,
    Corrupt,
    ForeignIdentity,
    Incompatible,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq, thiserror::Error)]
#[error("manifest unavailable ({reason:?}): {message}")]
pub struct ManifestUnavailable {
    pub code: &'static str,
    pub reason: ManifestUnavailableReason,
    pub actual_generation: Option<u64>,
    pub expected_generation: u64,
    pub retryable: bool,
    pub message: String,
}

impl ManifestUnavailable {
    pub fn new(
        reason: ManifestUnavailableReason,
        generation: u64,
        message: impl Into<String>,
    ) -> Self {
        let retryable = !matches!(
            reason,
            ManifestUnavailableReason::Corrupt
                | ManifestUnavailableReason::ForeignIdentity
                | ManifestUnavailableReason::Incompatible
                | ManifestUnavailableReason::SourceUnavailable
        );
        Self {
            code: if retryable {
                "manifest_temporarily_unavailable"
            } else {
                "manifest_unavailable"
            },
            reason,
            actual_generation: None,
            expected_generation: generation,
            retryable,
            message: message.into(),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ManifestRecoveryStatus {
    pub state: &'static str,
    pub attempts: u32,
    pub retry_after_seconds: Option<u64>,
    pub error: Option<ManifestUnavailable>,
    pub revision: u64,
}

/// Shared read-side status and wakeup only; the daemon is the sole job owner.
pub struct ManifestRecoveryRuntime {
    status: std::sync::Mutex<ManifestRecoveryStatus>,
    pub wake: tokio::sync::Notify,
    pub changed: tokio::sync::watch::Sender<u64>,
}
impl Default for ManifestRecoveryRuntime {
    fn default() -> Self {
        Self {
            status: std::sync::Mutex::new(ManifestRecoveryStatus {
                state: "queued",
                attempts: 0,
                retry_after_seconds: Some(2),
                error: None,
                revision: 0,
            }),
            wake: tokio::sync::Notify::new(),
            changed: tokio::sync::watch::channel(0).0,
        }
    }
}
impl ManifestRecoveryRuntime {
    pub fn status(&self) -> ManifestRecoveryStatus {
        self.status
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
    pub fn publish(
        &self,
        state: &'static str,
        attempts: u32,
        delay: Option<u64>,
        error: Option<ManifestUnavailable>,
    ) {
        let mut status = self.status.lock().unwrap_or_else(|e| e.into_inner());
        if status.state == state
            && status.attempts == attempts
            && status.retry_after_seconds == delay
            && status.error == error
        {
            return;
        }
        status.revision = status.revision.saturating_add(1);
        status.state = state;
        status.attempts = attempts;
        status.retry_after_seconds = delay;
        status.error = error;
        self.changed.send_replace(status.revision);
    }
}

pub fn manifest_debt_path(db_path: &Path) -> PathBuf {
    crate::sidecar_path(db_path, ".manifest-debt.json")
}

/// Written before source invalidation. Its unique revision prevents an old
/// repair from clearing a newer edit, including edits at the same generation.
pub fn mark_manifest_reconciliation_pending(db_path: &Path, reason: &str) -> anyhow::Result<()> {
    let bytes = serde_json::to_vec(&serde_json::json!({
        "revision": uuid::Uuid::new_v4().to_string(), "reason": reason,
    }))?;
    atomic_replace_file(&manifest_debt_path(db_path), |file| file.write_all(&bytes))?;
    Ok(())
}

pub fn manifest_debt_revision(db_path: &Path) -> anyhow::Result<Option<Vec<u8>>> {
    match std::fs::File::open(manifest_debt_path(db_path)) {
        Ok(file) => {
            let mut bytes = Vec::new();
            file.take(8193).read_to_end(&mut bytes)?;
            anyhow::ensure!(bytes.len() <= 8192, "manifest debt exceeds 8 KiB");
            let value: serde_json::Value = serde_json::from_slice(&bytes)?;
            let revision = value
                .get("revision")
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow::anyhow!("manifest debt revision is absent"))?;
            uuid::Uuid::parse_str(revision)?;
            anyhow::ensure!(
                value
                    .get("reason")
                    .and_then(|v| v.as_str())
                    .is_some_and(|s| !s.is_empty()),
                "manifest debt reason is absent"
            );
            Ok(Some(bytes))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// This loader is for current suggestions: absence is only empty when the
/// authoritative repository inventory is empty. Errors retain typed causes.
pub fn current_manifest_snapshot(
    store: &nestweaver_store::GraphStore,
    db_path: &Path,
) -> Result<HashMap<String, ManifestInfo>, ManifestUnavailable> {
    use ManifestUnavailableReason::*;
    use nestweaver_store::artifact_envelope::{
        ArtifactEnvelope, ArtifactExpectation, ArtifactRejection,
    };
    let generation = store.graph_generation();
    let failure = |reason, message: String| ManifestUnavailable::new(reason, generation, message);
    if store.is_index_publication_dirty() {
        return Err(failure(
            PublicationInProgress,
            "graph publication is incomplete".into(),
        ));
    }
    let repos = store
        .list_repos(None)
        .map_err(|e| failure(SourceUnavailable, e.to_string()))?;
    let path = manifest_cache_path(db_path);
    if std::fs::metadata(&path).is_ok_and(|m| m.len() > 64 * 1024 * 1024) {
        return Err(failure(
            Corrupt,
            "manifest artifact exceeds the 64 MiB read bound".into(),
        ));
    }
    let bounded_read = std::fs::File::open(&path).and_then(|file| {
        let mut bytes = Vec::new();
        file.take(64 * 1024 * 1024 + 1).read_to_end(&mut bytes)?;
        if bytes.len() > 64 * 1024 * 1024 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "manifest artifact exceeds 64 MiB",
            ));
        }
        Ok(bytes)
    });
    let bytes = match bounded_read {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound && repos.is_empty() => {
            if manifest_debt_revision(db_path)
                .map_err(|e| failure(Corrupt, format!("cannot inspect manifest debt: {e}")))?
                .is_some()
            {
                return Err(failure(
                    PendingSourceChange,
                    "source changes await complete manifest derivation".into(),
                ));
            }
            ensure_manifest_generation(store, generation)?;
            return Ok(HashMap::new());
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(failure(
                Missing,
                "manifest snapshot has not been published".into(),
            ));
        }
        Err(e) => {
            return Err(failure(
                Corrupt,
                format!("cannot read manifest snapshot: {e}"),
            ));
        }
    };
    let envelope: ArtifactEnvelope = serde_json::from_slice(&bytes)
        .map_err(|e| failure(Corrupt, format!("invalid artifact envelope: {e}")))?;
    let identity = store
        .publication_identity()
        .map_err(|e| failure(Incompatible, e.to_string()))?
        .ok_or_else(|| failure(Incompatible, "graph publication identity is absent".into()))?;
    let manifests: HashMap<String, ManifestInfo> = envelope
        .validate_and_decode_typed(ArtifactExpectation {
            artifact_kind: MANIFEST_ARTIFACT_KIND,
            artifact_schema_version: MANIFEST_ARTIFACT_SCHEMA_VERSION,
            identity: &identity,
            producer_version: env!("CARGO_PKG_VERSION"),
            source_graph_generation: generation,
            algorithm_fingerprint: MANIFEST_ALGORITHM_FINGERPRINT,
        })
        .map_err(|e| {
            let reason = match &e {
                ArtifactRejection::StaleGeneration { .. } => StaleGeneration,
                ArtifactRejection::ProducerChanged { .. } => ProducerChanged,
                ArtifactRejection::ForeignIdentity => ForeignIdentity,
                ArtifactRejection::Corrupt(_) => Corrupt,
                ArtifactRejection::Incompatible(_) => Incompatible,
            };
            let mut error = failure(reason, e.to_string());
            error.actual_generation = Some(envelope.source_graph_generation);
            error
        })?;
    if manifest_debt_revision(db_path)
        .map_err(|e| failure(Corrupt, format!("cannot inspect manifest debt: {e}")))?
        .is_some()
    {
        return Err(failure(
            PendingSourceChange,
            "source changes await complete manifest derivation".into(),
        ));
    }
    if repos.len() != manifests.len() || repos.iter().any(|r| !manifests.contains_key(&r.uid)) {
        return Err(failure(
            IncompleteCoverage,
            "manifest snapshot does not cover the live repository inventory".into(),
        ));
    }
    ensure_manifest_generation(store, generation)?;
    Ok(manifests)
}

pub fn ensure_manifest_generation(
    store: &nestweaver_store::GraphStore,
    generation: u64,
) -> Result<(), ManifestUnavailable> {
    if store.is_index_publication_dirty() || generation != store.graph_generation() {
        let mut error = ManifestUnavailable::new(
            ManifestUnavailableReason::PublicationInProgress,
            store.graph_generation(),
            "graph changed while computing suggestions; retry after publication",
        );
        error.actual_generation = Some(generation);
        return Err(error);
    }
    Ok(())
}

/// Immutable eligible input bytes. Reusing the normal parser against this
/// reader removes fallible I/O from format fallback and allows exact rechecks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManifestInputs {
    root: PathBuf,
    files: std::collections::BTreeMap<PathBuf, String>,
}
impl ContentReader for ManifestInputs {
    fn read_file(&self, path: &Path) -> anyhow::Result<String> {
        self.files
            .get(path)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("manifest absent: {}", path.display()))
    }
    fn list_files(&self) -> anyhow::Result<Vec<PathBuf>> {
        Ok(self.files.keys().cloned().collect())
    }
    fn file_meta_nanos(&self, _: &Path) -> anyhow::Result<Option<(u64, u64)>> {
        Ok(None)
    }
    fn root(&self) -> &Path {
        &self.root
    }
    fn version_id(&self) -> &str {
        "captured-manifests"
    }
}

impl ManifestInputs {
    pub fn digest(&self) -> String {
        let mut hash = blake3::Hasher::new();
        for (path, content) in &self.files {
            let path = path.to_string_lossy();
            hash.update(&(path.len() as u64).to_le_bytes());
            hash.update(path.as_bytes());
            hash.update(&(content.len() as u64).to_le_bytes());
            hash.update(content.as_bytes());
        }
        hash.finalize().to_hex().to_string()
    }
}

pub fn is_manifest_input(path: &Path) -> bool {
    let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
    name == "package.json"
        || (path.components().count() <= 2 && path.extension().is_some_and(|s| s == "csproj"))
        || (path.components().count() == 1
            && [
                "Cargo.toml",
                "go.mod",
                "pyproject.toml",
                "requirements.txt",
                "composer.json",
                "Gemfile",
                "pubspec.yaml",
                "Package.swift",
                "build.gradle.kts",
                "CMakeLists.txt",
            ]
            .contains(&name))
}

pub fn capture_manifest_inputs(
    reader: &dyn ContentReader,
    remaining_bytes: &mut usize,
    deadline: std::time::Instant,
) -> anyhow::Result<ManifestInputs> {
    let mut files = std::collections::BTreeMap::new();
    let inventory = reader.list_files()?;
    for path in inventory.into_iter().filter(|p| is_manifest_input(p)) {
        anyhow::ensure!(
            std::time::Instant::now() < deadline,
            "manifest derivation exceeded its 60-second cooperative deadline"
        );
        let content = reader.read_file(&path)?;
        anyhow::ensure!(
            content.len() <= *remaining_bytes,
            "manifest capture exceeds the remaining bounded snapshot budget"
        );
        *remaining_bytes -= content.len();
        validation::validate(&path, &content)
            .map_err(|error| anyhow::anyhow!("{}: {error:#}", path.display()))?;
        files.insert(path, content);
    }
    anyhow::ensure!(
        std::time::Instant::now() < deadline,
        "manifest derivation exceeded its 60-second cooperative deadline"
    );
    Ok(ManifestInputs {
        root: reader.root().to_path_buf(),
        files,
    })
}

/// A dead-code run must disclose any loss of current manifest entry roots.
/// Absence is legitimately empty only when the graph has no repositories.
/// Pending source debt, incomplete coverage, dirty publication and envelope
/// failures all prevent old roots from being presented as current.
#[derive(Debug, Default, Clone)]
pub struct DeadCodeManifests {
    /// Current manifests keyed by repo UID, or empty with `load_error` on failure.
    pub manifests: HashMap<String, ManifestInfo>,
    /// Disclosure for unavailable current roots. None means complete coverage.
    pub load_error: Option<String>,
}

impl DeadCodeManifests {
    /// The disclosure a caller should attach to its dead-code response, if any.
    pub fn disclosure(&self) -> Option<&str> {
        self.load_error.as_deref()
    }
}

/// Load the manifest sidecar for a dead-code run on ANY route.
///
/// nw-512. The direct CLI path loaded manifests and the MCP `dead_code` tool
/// did not, so the daemon CLI, MCP-direct and MCP-via-daemon all walked with
/// an EMPTY manifest map while `nestweaver dead-code --no-daemon` walked with
/// a populated one — four routes, two answers, same database. This is the one
/// loader all of them call, so they cannot drift again.
///
/// Never returns `Err`: a dead-code run over a graph whose manifest sidecar
/// cannot be read is still a legitimate (if degraded) answer, and refusing it
/// outright would be a bigger behaviour change than the silent degradation
/// this replaces. The failure travels in `load_error` instead — see
/// [`DeadCodeManifests`].
pub fn load_manifests_for_dead_code(
    store: &nestweaver_store::GraphStore,
    db_path: &Path,
) -> DeadCodeManifests {
    match current_manifest_snapshot(store, db_path) {
        Ok(manifests) => DeadCodeManifests {
            manifests,
            load_error: None,
        },
        Err(error) => DeadCodeManifests {
            manifests: HashMap::new(),
            load_error: Some(format!(
                "manifest sidecar {} could not be read ({error:#}); manifest-declared entry \
                 files did NOT seed the reachability walk, so code reachable only from a \
                 package entry point may appear unreachable. The daemon reports manifest \
                 reconciliation progress through the suggestions endpoint.",
                manifest_cache_path(db_path).display()
            )),
        },
    }
}

/// Publication status for a graph mutation that has already committed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GraphMutationPublicationDisposition {
    /// The store proved that no graph state changed.
    ConfirmedNoChange,
    /// The mutation and every required derived-artifact reconciliation step
    /// completed.
    CommittedComplete,
    /// The graph mutation committed, but one or more reconciliation steps
    /// failed and require operator repair.
    CommittedDegraded,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GraphMutationPublicationWarning {
    pub stage: String,
    pub message: String,
}

/// Complete publication result for a graph mutation.
///
/// A committed graph write is never turned back into a plain `Err` merely
/// because generation persistence or sidecar reconciliation subsequently
/// failed. Callers can therefore report the truthful committed-but-degraded
/// state instead of implying that the graph rolled back.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GraphMutationPublicationOutcome {
    pub disposition: GraphMutationPublicationDisposition,
    pub generation_before: u64,
    pub generation_after: u64,
    pub warnings: Vec<GraphMutationPublicationWarning>,
}

/// Crash fence for a non-index graph mutation.
///
/// Unlike a full index publication this guard does not reserve dirty N+1 and
/// clean N+2 generations: the materialization contract requires exactly one
/// successor generation. The durable marker still brackets the graph commit,
/// so interruption before post-commit reconciliation leaves every ranked read
/// and snapshot fail-closed until normal abandoned-publication repair runs.
pub struct GraphMutationPublicationGuard<'a> {
    lease: Option<nestweaver_store::IndexPublicationLease<'a>>,
    marker_path: Option<PathBuf>,
    operation: String,
}

impl<'a> GraphMutationPublicationGuard<'a> {
    /// Finish a bracketed operation. A confirmed no-op retires the marker
    /// without advancing generation; a commit publishes exactly once.
    pub fn finish(
        mut self,
        changed: bool,
    ) -> Result<GraphMutationPublicationOutcome, anyhow::Error> {
        let store = self
            .lease
            .as_ref()
            .expect("publication guard always owns its lease until finish")
            .store();
        let mut outcome = finalize_committed_graph_mutation(store, changed);

        if !outcome.is_degraded()
            && let Some(marker_path) = &self.marker_path
            && let Err(error) = store.with_index_publication_rank_barrier(|| {
                nestweaver_store::durable_sidecar::remove_file_durable_if_exists(marker_path)
            })
        {
            if changed {
                outcome.record_warning(
                    "retire-graph-publication-marker",
                    format!(
                        "{} committed clean state but could not retire {}: {error}",
                        self.operation,
                        marker_path.display()
                    ),
                );
            } else {
                anyhow::bail!(
                    "{} made no graph change but could not retire publication marker {}: {error}",
                    self.operation,
                    marker_path.display()
                );
            }
        }

        if let Some(lease) = self.lease.take()
            && let Err(error) = lease.release()
        {
            if changed {
                outcome.record_warning(
                    "release-graph-publication-lease",
                    format!(
                        "{} committed but could not release its publication lease: {error}",
                        self.operation
                    ),
                );
            } else {
                return Err(anyhow::anyhow!(
                    "{} made no graph change but could not release its publication lease: {error}",
                    self.operation
                ));
            }
        }
        Ok(outcome)
    }
}

/// Durably mark a graph publication dirty before its first possible write.
///
/// The returned guard must span the graph commit and post-commit finalizer.
/// Dropping it early intentionally releases only live ownership; the durable
/// marker remains so a crash or ambiguous write cannot make old derived data
/// authoritative.
pub fn begin_graph_mutation_publication<'a>(
    store: &'a nestweaver_store::GraphStore,
    operation: impl Into<String>,
) -> Result<GraphMutationPublicationGuard<'a>, anyhow::Error> {
    let operation = operation.into();
    let lease = store
        .acquire_index_publication_lease()
        .map_err(|error| anyhow::anyhow!("{operation}: acquire publication lease: {error}"))?;
    lease.ensure_clean_for_snapshot().map_err(|error| {
        anyhow::anyhow!("{operation}: refusing to overwrite a dirty publication: {error}")
    })?;

    let marker_path = if let Some(db_path) = store.db_path() {
        lease.preflight_generation().map_err(|error| {
            anyhow::anyhow!("{operation}: preflight successor generation: {error}")
        })?;
        let marker_path = crate::sidecar_path(db_path, ".index-dirty");
        let payload = nestweaver_store::index_publication::format_marker_payload(
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos(),
            None,
        );
        store
            .with_index_publication_rank_barrier(|| {
                nestweaver_store::durable_sidecar::atomic_replace_file(&marker_path, |file| {
                    file.write_all(payload.as_bytes())
                })
            })
            .map_err(|error| {
                anyhow::anyhow!(
                    "{operation}: publish graph mutation marker {}: {error}",
                    marker_path.display()
                )
            })?;
        Some(marker_path)
    } else {
        lease.preflight_transient_generation().map_err(|error| {
            anyhow::anyhow!("{operation}: preflight in-memory successor generation: {error}")
        })?;
        None
    };

    Ok(GraphMutationPublicationGuard {
        lease: Some(lease),
        marker_path,
        operation,
    })
}

impl GraphMutationPublicationOutcome {
    pub fn changed(&self) -> bool {
        self.disposition != GraphMutationPublicationDisposition::ConfirmedNoChange
    }

    pub fn is_degraded(&self) -> bool {
        self.disposition == GraphMutationPublicationDisposition::CommittedDegraded
    }

    pub fn record_warning(&mut self, stage: impl Into<String>, message: impl Into<String>) {
        self.warnings.push(GraphMutationPublicationWarning {
            stage: stage.into(),
            message: message.into(),
        });
        if self.disposition == GraphMutationPublicationDisposition::CommittedComplete {
            self.disposition = GraphMutationPublicationDisposition::CommittedDegraded;
        }
    }
}

/// Publish one already-committed graph mutation.
///
/// A confirmed no-op leaves every generation and derived artifact untouched.
/// A change invalidates both live and durable PageRank, advances generation
/// exactly once, persists it, and rebinds a valid repository-manifest cache to
/// the successor generation. Post-commit failures are accumulated instead of
/// being returned as rollback-looking errors.
pub fn finalize_committed_graph_mutation(
    store: &nestweaver_store::GraphStore,
    changed: bool,
) -> GraphMutationPublicationOutcome {
    let generation_before = store.graph_generation();
    if !changed {
        return GraphMutationPublicationOutcome {
            disposition: GraphMutationPublicationDisposition::ConfirmedNoChange,
            generation_before,
            generation_after: generation_before,
            warnings: Vec::new(),
        };
    }

    let db_path = store.db_path().map(Path::to_path_buf);
    let mut outcome = GraphMutationPublicationOutcome {
        disposition: GraphMutationPublicationDisposition::CommittedComplete,
        generation_before,
        generation_after: generation_before,
        warnings: Vec::new(),
    };
    let carried_manifests = db_path.as_ref().and_then(|db_path| {
        let path = manifest_cache_path(db_path);
        if !path.exists() {
            return None;
        }
        match load_manifest_cache_for_db(store, db_path) {
            Ok(manifests) => Some(manifests),
            Err(error) => {
                // nw-459. The manifest cache is REBUILDABLE derived data. An
                // upgrade invalidates it by construction (the artifact records
                // the producing version), and the next index regenerates it.
                // Recording that as a publication WARNING flipped the
                // disposition to `CommittedDegraded`, which the daemon and CLI
                // both surface as a hard error — so the first index after every
                // upgrade exited non-zero on a graph that had committed
                // perfectly, telling the user to "repair the named stage(s)"
                // for a stage that needed no repair.
                //
                // A genuine incompatibility (different algorithm, different
                // schema, foreign identity, corrupt payload) is NOT rebuildable
                // and still degrades the publication loudly.
                let rendered = format!("{error:#}");
                if nestweaver_store::artifact_envelope::is_rebuildable_artifact(&rendered) {
                    tracing::debug!(
                        error = %rendered,
                        "dropping a rebuildable manifest cache; the next index regenerates it"
                    );
                } else {
                    outcome.record_warning(
                        "load-manifest-cache",
                        format!(
                            "could not carry the repository manifest cache across publication: {rendered}"
                        ),
                    );
                }
                None
            }
        }
    });

    if let Err(error) = store.reconcile_embedding_index() {
        outcome.record_warning(
            "embedding-index",
            format!("graph committed; embedding reconciliation must be retried: {error:#}"),
        );
    }

    // Clear live scores before exposing the new generation. The durable copy
    // is removed as well, so a process restart cannot reload pre-mutation
    // ranking even if a later publication step is degraded.
    store.invalidate_pagerank();
    if let Some(db_path) = &db_path {
        let pagerank_path = crate::sidecar_path(db_path, ".pagerank.json");
        if let Err(error) =
            nestweaver_store::durable_sidecar::remove_file_durable_if_exists(&pagerank_path)
        {
            outcome.record_warning(
                "invalidate-pagerank-sidecar",
                format!(
                    "could not durably remove stale PageRank sidecar {}: {error}",
                    pagerank_path.display()
                ),
            );
        }
    }

    let generation_after = match store.try_bump_graph_generation() {
        Ok(generation) => generation,
        Err(error) => {
            outcome.record_warning(
                "advance-graph-generation",
                format!("committed graph mutation could not advance generation: {error}"),
            );
            outcome.generation_after = store.graph_generation();
            return outcome;
        }
    };
    outcome.generation_after = generation_after;

    let generation_persisted = if let Some(db_path) = &db_path {
        let generation_path = crate::sidecar_path(db_path, ".generation");
        match store.save_graph_generation(&generation_path) {
            Ok(()) => true,
            Err(error) => {
                outcome.record_warning(
                    "persist-graph-generation",
                    format!(
                        "generation {generation_after} is live but could not be durably published to {}: {error}",
                        generation_path.display()
                    ),
                );
                false
            }
        }
    } else {
        true
    };

    if generation_persisted
        && let (Some(db_path), Some(manifests)) = (&db_path, carried_manifests)
        && let Err(error) = save_manifest_cache_for_db(&manifests, store, db_path)
    {
        outcome.record_warning(
            "rebind-manifest-cache",
            format!(
                "could not rebind the repository manifest cache to generation {generation_after}: {error:#}"
            ),
        );
    }

    for warning in &outcome.warnings {
        tracing::warn!(
            stage = %warning.stage,
            message = %warning.message,
            generation_before,
            generation_after = outcome.generation_after,
            "graph mutation committed with degraded publication reconciliation"
        );
    }
    outcome
}

/// Advance the graph generation while carrying the manifest cache across the
/// boundary.
///
/// nw-289, deeper property. `.manifests.json` is identity- AND
/// generation-bound: its envelope records `source_graph_generation`, and
/// [`load_manifest_cache_for_db`] refuses to decode it once that no longer
/// matches the live graph. Only the DELETION path reconciled that; every other
/// generation advance left the artifact orphaned.
///
/// Measured before the fix: a code index leaves graph and manifest cache both
/// at generation 2; a subsequent `brain add` moves the graph to 3, the
/// manifest cache stays at 2, and from then on
/// `load_manifest_cache_for_db` fails with "stale artifact generation" until a
/// code index happens to run. Every CLI consumer loads it with
/// `.unwrap_or_default()`, so the graph does not report an error — `dead-code`
/// quietly loses its manifest-driven entry points and `suggest-links` its
/// cross-repo signal.
///
/// REPUBLISH, not invalidate. A markdown index cannot change a code manifest,
/// so the payload is still correct and only its binding went stale; deleting
/// the artifact would force a full code re-index to recover data that was
/// never wrong. The deletion path is the one case whose payload genuinely must
/// be FILTERED first, and it still does that separately.
///
/// Taking the advance as a closure is the point: the read must happen before
/// it and the write after, and the deletion path's own comment records what
/// the other ordering costs — "saving at N and then advancing to N+1 makes a
/// freshly written, identity-bound artifact stale immediately". A helper that
/// only did the write could be called in the wrong place; this one cannot be.
///
/// Read failures are ignored by design. An absent cache is the normal state
/// for a graph with no code repos, and an ALREADY-stale or corrupt one must
/// not be re-blessed against a generation it was never derived from — both
/// simply skip the republish. A WRITE failure is logged rather than returned,
/// because the graph mutation is already committed and the caller cannot undo
/// it; the artifact is left stale, which is the pre-existing behaviour.
pub(crate) fn advancing_generation_rebinding_manifests<T>(
    store: &nestweaver_store::GraphStore,
    advance: impl FnOnce() -> T,
) -> T {
    let carried = store.db_path().and_then(|db_path| {
        load_manifest_cache_for_db(store, db_path)
            .ok()
            .filter(|manifests| !manifests.is_empty())
            .map(|manifests| (db_path.to_path_buf(), manifests))
    });
    let outcome = advance();
    if let Some((db_path, manifests)) = carried
        && let Err(error) = save_manifest_cache_for_db(&manifests, store, &db_path)
    {
        tracing::warn!(
            "could not rebind the manifest cache to the published generation: {error:#};              it stays bound to the previous one and a code re-index will restore it"
        );
    }
    outcome
}

/// Persist the canonical manifest sidecar and retire the legacy copy only
/// after the replacement has been durably flushed and renamed into place.
pub fn save_manifest_cache_for_db(
    manifests: &HashMap<String, ManifestInfo>,
    store: &nestweaver_store::GraphStore,
    db_path: &Path,
) -> Result<(), anyhow::Error> {
    #[cfg(feature = "release-fixture-hooks")]
    crate::release_fixture::manifest_before_save()?;
    let canonical_path = manifest_cache_path(db_path);
    crate::artifact_sidecar::save_json(
        store,
        &canonical_path,
        MANIFEST_ARTIFACT_KIND,
        MANIFEST_ARTIFACT_SCHEMA_VERSION,
        MANIFEST_ALGORITHM_FINGERPRINT,
        manifests,
    )?;

    let legacy_path = db_path.with_extension("manifests.json");
    if legacy_path != canonical_path {
        nestweaver_store::durable_sidecar::remove_file_durable_if_exists(&legacy_path).map_err(
            |error| {
                anyhow::anyhow!(
                    "durably remove legacy manifest sidecar {}: {error}",
                    legacy_path.display()
                )
            },
        )?;
    }
    Ok(())
}

pub(crate) fn atomic_replace_file(
    path: &Path,
    write: impl FnOnce(&mut std::fs::File) -> std::io::Result<()>,
) -> Result<(), anyhow::Error> {
    nestweaver_store::durable_sidecar::atomic_replace_file(path, write).map_err(Into::into)
}

/// Load a `HashMap<repo_uid, ManifestInfo>` from a JSON sidecar file.
///
/// Returns an empty map when the file does not exist.
pub fn load_manifest_cache(path: &Path) -> Result<HashMap<String, ManifestInfo>, anyhow::Error> {
    if !path.exists() {
        return Ok(HashMap::new());
    }
    let json = std::fs::read_to_string(path)?;
    let map = serde_json::from_str(&json)?;
    Ok(map)
}

// ── per-format parsers ────────────────────────────────────────────────────────

fn parse_package_json(reader: &dyn ContentReader) -> Option<ManifestInfo> {
    let content = reader.read_file(Path::new("package.json")).ok()?;
    let json: serde_json::Value = serde_json::from_str(&content).ok()?;

    let package_name = json.get("name")?.as_str().map(String::from);

    let mut deps = Vec::new();
    for field in ["dependencies", "devDependencies", "peerDependencies"] {
        if let Some(obj) = json.get(field).and_then(|v| v.as_object()) {
            deps.extend(obj.keys().cloned());
        }
    }

    // `entry_files` is populated separately by `discover_package_json_entry_files`,
    // which walks every package.json in the repo (root included) independent of
    // whether it has a `name` — see that function's doc comment.
    Some(ManifestInfo {
        package_name,
        dependencies: deps,
        entry_files: Vec::new(),
    })
}

/// Recursively collect string values from the `exports` field of package.json.
/// The `exports` field can be a string, an array (Node's documented fallback
/// list — the first entry a resolver understands wins at runtime, but every
/// entry is a candidate entry point, and over-rooting a symbol is the safe
/// direction for a deletion aid), an object with condition keys mapping to
/// strings/arrays/nested objects, or an object with subpath keys.
fn collect_export_paths(value: &serde_json::Value, out: &mut Vec<String>) {
    match value {
        serde_json::Value::String(s) => out.push(s.clone()),
        serde_json::Value::Object(obj) => {
            for v in obj.values() {
                collect_export_paths(v, out);
            }
        }
        serde_json::Value::Array(arr) => {
            for v in arr {
                collect_export_paths(v, out);
            }
        }
        _ => {}
    }
}

/// Extract the raw (as-written, not yet rebased) entry-point strings from one
/// parsed `package.json` document: `main`, `bin` (string or object), `exports`
/// (string or nested condition/subpath map), and `browser` only when it is a
/// string — the object form is a bundler replacement map, not a documented
/// npm/Node entry point.
fn collect_package_json_raw_entries(json: &serde_json::Value) -> Vec<String> {
    let mut entries = Vec::new();
    if let Some(main) = json.get("main").and_then(|v| v.as_str()) {
        entries.push(main.to_string());
    }
    if let Some(bin) = json.get("bin") {
        match bin {
            serde_json::Value::String(s) => entries.push(s.clone()),
            serde_json::Value::Object(obj) => {
                for v in obj.values() {
                    if let Some(s) = v.as_str() {
                        entries.push(s.to_string());
                    }
                }
            }
            _ => {}
        }
    }
    if let Some(exports) = json.get("exports") {
        collect_export_paths(exports, &mut entries);
    }
    if let Some(browser) = json.get("browser").and_then(|v| v.as_str()) {
        entries.push(browser.to_string());
    }
    entries
}

/// Rebase one raw `package.json` entry path onto the repo-root-relative
/// directory containing that manifest, producing a `/`-joined, lexically
/// normalized path (`.` dropped, `..` resolved against what came before it).
///
/// Returns `None` for anything that cannot be a literal repo-relative file
/// path:
/// - empty or whitespace-only (on a nested manifest this would otherwise
///   rebase to the manifest's own directory, which is not a file);
/// - contains a `*` (a subpath-pattern target, not a literal file —
///   expanding it is out of scope);
/// - starts with `/` (an absolute filesystem path, never repo-relative —
///   rebasing it under the manifest's directory would be silently wrong,
///   not merely inert);
/// - contains `\` or a `://` scheme (a Windows-style path or a URL; neither
///   is ever a repo-relative path here — Windows paths are unsupported);
/// - ends with `/` (a directory reference, e.g. `"lib/"`; npm resolves a
///   directory `main`/`exports` target via its own `index.js`/`package.json`
///   lookup, which this function does not implement — filed as follow-up
///   nw-499, not silently mis-rooted as the literal directory name);
/// - a leading `..` that would walk outside the repo root (nothing left to
///   pop).
fn rebase_package_json_entry(manifest_dir: &Path, entry: &str) -> Option<String> {
    let entry = entry.trim();
    if entry.is_empty()
        || entry.contains('*')
        || entry.starts_with('/')
        || entry.contains('\\')
        || entry.contains("://")
        || entry.ends_with('/')
    {
        return None;
    }
    let mut segments: Vec<&str> = manifest_dir
        .iter()
        .filter_map(|c| c.to_str())
        .filter(|s| !s.is_empty())
        .collect();
    for part in entry.split('/') {
        match part {
            "" | "." => continue,
            ".." => {
                segments.pop()?;
            }
            other => segments.push(other),
        }
    }
    if segments.is_empty() {
        return None;
    }
    Some(segments.join("/"))
}

/// Discover every `package.json` in the repo, at any depth and including the
/// root, and return the union of their rebased, repo-relative entry-point
/// paths.
///
/// Reuses `reader.list_files()`, the same repo-relative file listing the
/// index's own file walk uses, so `node_modules` and the other shared
/// skip-dirs, plus `.gitignore`/`.git/info/exclude`, are already applied —
/// no separate exclusion logic is needed here. The root is included
/// deliberately (not skipped as "already covered" by [`parse_package_json`]):
/// a root `package.json` without a `name` field is invisible to
/// `parse_package_json`, and this is the only place its entries are
/// recovered.
fn discover_package_json_entry_files(reader: &dyn ContentReader) -> Vec<String> {
    let Ok(files) = reader.list_files() else {
        return Vec::new();
    };
    let mut entries = Vec::new();
    for path in files {
        if path.file_name().and_then(|n| n.to_str()) != Some("package.json") {
            continue;
        }
        let Ok(content) = reader.read_file(&path) else {
            continue;
        };
        let Ok(json) = serde_json::from_str::<serde_json::Value>(&content) else {
            continue;
        };
        let manifest_dir = path.parent().unwrap_or(Path::new(""));
        for raw in collect_package_json_raw_entries(&json) {
            if let Some(rebased) = rebase_package_json_entry(manifest_dir, &raw)
                && !entries.contains(&rebased)
            {
                entries.push(rebased);
            }
        }
    }
    entries
}

fn parse_go_mod(reader: &dyn ContentReader) -> Option<ManifestInfo> {
    let content = reader.read_file(Path::new("go.mod")).ok()?;
    validation::go_manifest(&content).ok()
}

fn parse_cargo_toml(reader: &dyn ContentReader) -> Option<ManifestInfo> {
    let content = reader.read_file(Path::new("Cargo.toml")).ok()?;
    let toml: toml::Value = toml::from_str(&content).ok()?;

    let package_name = toml.get("package")?.get("name")?.as_str().map(String::from);

    let mut deps = Vec::new();
    for section in ["dependencies", "dev-dependencies", "build-dependencies"] {
        if let Some(table) = toml.get(section).and_then(|v| v.as_table()) {
            deps.extend(table.keys().cloned());
        }
    }

    Some(ManifestInfo {
        package_name,
        dependencies: deps,
        entry_files: vec![],
    })
}

fn parse_pyproject_toml(reader: &dyn ContentReader) -> Option<ManifestInfo> {
    let content = reader.read_file(Path::new("pyproject.toml")).ok()?;
    let toml: toml::Value = toml::from_str(&content).ok()?;

    let package_name = toml.get("project")?.get("name")?.as_str().map(String::from);

    let mut deps = Vec::new();
    if let Some(dep_list) = toml
        .get("project")
        .and_then(|p| p.get("dependencies"))
        .and_then(|d| d.as_array())
    {
        for dep in dep_list {
            if let Some(s) = dep.as_str() {
                // PEP 508: "package-name>=1.0" — extract name before version specifier
                let name = s
                    .split(|c: char| !c.is_alphanumeric() && c != '-' && c != '_' && c != '.')
                    .next()
                    .unwrap_or(s);
                deps.push(name.to_string());
            }
        }
    }

    Some(ManifestInfo {
        package_name,
        dependencies: deps,
        entry_files: vec![],
    })
}

fn parse_requirements_txt(reader: &dyn ContentReader) -> Option<ManifestInfo> {
    let content = reader.read_file(Path::new("requirements.txt")).ok()?;

    let mut deps = Vec::new();
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') || trimmed.starts_with('-') {
            continue;
        }
        let name = trimmed
            .split(|c: char| !c.is_alphanumeric() && c != '-' && c != '_' && c != '.')
            .next()
            .unwrap_or(trimmed);
        if !name.is_empty() {
            deps.push(name.to_string());
        }
    }

    if deps.is_empty() {
        return None;
    }
    Some(ManifestInfo {
        package_name: None,
        dependencies: deps,
        entry_files: vec![],
    })
}

fn parse_composer_json(reader: &dyn ContentReader) -> Option<ManifestInfo> {
    let content = reader.read_file(Path::new("composer.json")).ok()?;
    let json: serde_json::Value = serde_json::from_str(&content).ok()?;

    let package_name = json.get("name").and_then(|v| v.as_str()).map(String::from);

    let mut deps = Vec::new();
    for field in ["require", "require-dev"] {
        if let Some(obj) = json.get(field).and_then(|v| v.as_object()) {
            for key in obj.keys() {
                if key != "php" && !key.starts_with("ext-") {
                    deps.push(key.clone());
                }
            }
        }
    }

    Some(ManifestInfo {
        package_name,
        dependencies: deps,
        entry_files: vec![],
    })
}

fn parse_gemfile(reader: &dyn ContentReader) -> Option<ManifestInfo> {
    let content = reader.read_file(Path::new("Gemfile")).ok()?;

    let mut deps = Vec::new();
    for line in content.lines() {
        let trimmed = line.trim();
        if let Some(after) = trimmed
            .strip_prefix("gem ")
            .or_else(|| trimmed.strip_prefix("gem("))
        {
            let name = after
                .trim_start_matches(['\'', '"'])
                .split(['\'', '"'])
                .next()
                .unwrap_or("");
            if !name.is_empty() {
                deps.push(name.to_string());
            }
        }
    }

    if deps.is_empty() {
        return None;
    }
    Some(ManifestInfo {
        package_name: None,
        dependencies: deps,
        entry_files: vec![],
    })
}

fn parse_pubspec_yaml(reader: &dyn ContentReader) -> Option<ManifestInfo> {
    let content = reader.read_file(Path::new("pubspec.yaml")).ok()?;
    let yaml: serde_yaml::Value = serde_yaml::from_str(&content).ok()?;

    let package_name = yaml.get("name").and_then(|v| v.as_str()).map(String::from);

    let mut deps = Vec::new();
    for field in ["dependencies", "dev_dependencies"] {
        if let Some(mapping) = yaml.get(field).and_then(|v| v.as_mapping()) {
            for key in mapping.keys() {
                if let Some(name) = key
                    .as_str()
                    .filter(|n| *n != "flutter" && *n != "flutter_test")
                {
                    deps.push(name.to_string());
                }
            }
        }
    }

    Some(ManifestInfo {
        package_name,
        dependencies: deps,
        entry_files: vec![],
    })
}

fn parse_package_swift(reader: &dyn ContentReader) -> Option<ManifestInfo> {
    let content = reader.read_file(Path::new("Package.swift")).ok()?;

    let package_name = content.lines().find_map(|line| {
        let trimmed = line.trim();
        if let Some(after) = trimmed.strip_prefix("name:") {
            let name = after
                .trim()
                .trim_matches(|c: char| c == '"' || c == ',' || c == ' ');
            if !name.is_empty() {
                return Some(name.to_string());
            }
        }
        None
    });

    let mut deps = Vec::new();
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.contains(".package(url:")
            && let Some(url_start) = trimmed.find("url:")
        {
            let after_url = &trimmed[url_start + 4..];
            let url = after_url
                .trim()
                .trim_start_matches([' ', '"'])
                .split('"')
                .next()
                .unwrap_or("");
            if let Some(last_segment) = url.rsplit('/').next() {
                let name = last_segment.trim_end_matches(".git");
                if !name.is_empty() {
                    deps.push(name.to_string());
                }
            }
        }
    }

    Some(ManifestInfo {
        package_name,
        dependencies: deps,
        entry_files: vec![],
    })
}

/// Find the first `.csproj` file in the repo root or one level of
/// subdirectories, using the reader's file listing instead of `read_dir`.
fn find_csproj(reader: &dyn ContentReader) -> Option<std::path::PathBuf> {
    let files = reader.list_files().ok()?;
    // Prefer root-level csproj files, then one-level subdirectory files.
    let mut root_level: Option<std::path::PathBuf> = None;
    let mut subdir_level: Option<std::path::PathBuf> = None;
    for f in &files {
        if f.extension().is_some_and(|ext| ext == "csproj") {
            let depth = f.components().count();
            if depth == 1 && root_level.is_none() {
                root_level = Some(f.clone());
            } else if depth == 2 && subdir_level.is_none() {
                subdir_level = Some(f.clone());
            }
        }
    }
    root_level.or(subdir_level)
}

fn parse_csproj(reader: &dyn ContentReader) -> Option<ManifestInfo> {
    let csproj_rel = find_csproj(reader)?;
    let content = reader.read_file(&csproj_rel).ok()?;

    let mut deps = Vec::new();
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.contains("PackageReference")
            && let Some(start) = trimmed.find("Include=\"")
        {
            let after = &trimmed[start + 9..];
            if let Some(end) = after.find('"') {
                let name = &after[..end];
                if !name.is_empty() {
                    deps.push(name.to_string());
                }
            }
        }
    }

    if deps.is_empty() {
        return None;
    }
    Some(ManifestInfo {
        package_name: None,
        dependencies: deps,
        entry_files: vec![],
    })
}

fn parse_build_gradle_kts(reader: &dyn ContentReader) -> Option<ManifestInfo> {
    let content = reader.read_file(Path::new("build.gradle.kts")).ok()?;

    let mut deps = Vec::new();
    for line in content.lines() {
        let trimmed = line.trim();
        for prefix in [
            "implementation(\"",
            "api(\"",
            "testImplementation(\"",
            "runtimeOnly(\"",
            "compileOnly(\"",
            "implementation('",
            "api('",
            "testImplementation('",
            "runtimeOnly('",
            "compileOnly('",
        ] {
            if let Some(after) = trimmed.strip_prefix(prefix) {
                let dep_str = after.split(['"', '\'']).next().unwrap_or("");
                let parts: Vec<&str> = dep_str.split(':').collect();
                if parts.len() >= 2 {
                    deps.push(format!("{}:{}", parts[0], parts[1]));
                }
            }
        }
    }

    if deps.is_empty() {
        return None;
    }
    Some(ManifestInfo {
        package_name: None,
        dependencies: deps,
        entry_files: vec![],
    })
}

fn parse_cmake(reader: &dyn ContentReader) -> Option<ManifestInfo> {
    let content = reader.read_file(Path::new("CMakeLists.txt")).ok()?;

    let mut package_name = None;
    let mut deps = Vec::new();

    for line in content.lines() {
        let trimmed = line.trim();
        if let Some(after) = trimmed.strip_prefix("project(") {
            let name = after.split([')', ' ']).next().unwrap_or("");
            if !name.is_empty() {
                package_name = Some(name.to_string());
            }
        }
        if let Some(after) = trimmed.strip_prefix("find_package(") {
            let name = after.split([')', ' ']).next().unwrap_or("");
            if !name.is_empty() {
                deps.push(name.to_string());
            }
        }
    }

    Some(ManifestInfo {
        package_name,
        dependencies: deps,
        entry_files: cmake_executable_sources(&content),
    })
}

/// Collect the source files named by every `add_executable(...)` in a
/// CMakeLists.txt.
///
/// nw-351: `parse_cmake` returned `entry_files: vec![]` while `Cargo.toml` and
/// `package.json` both contribute entry files, so a CMake project fed NOTHING
/// to `dead_code`'s manifest-driven seeding (`dead_code.rs:388`). Combined with
/// `detect_cpp` recognising only `main` and a handful of test macros, that is
/// how a real C++ corpus reached zero entry points and was reported 100% dead.
///
/// `add_executable` and not `add_library`, deliberately: a library's surface is
/// called from outside the corpus and is not modelled as an entry point today,
/// while an executable's sources genuinely are the program's roots.
///
/// The scan is over the whole file rather than line-by-line because the call is
/// conventionally wrapped across lines. Tokens that are CMake keywords, that
/// carry a `$` (an unresolved variable or generator expression), or that have
/// no file extension are dropped; the first token is the target NAME, never a
/// source.
fn cmake_executable_sources(content: &str) -> Vec<String> {
    const KEYWORDS: [&str; 4] = ["WIN32", "MACOSX_BUNDLE", "EXCLUDE_FROM_ALL", "IMPORTED"];
    let mut out = Vec::new();
    let mut rest = content;
    while let Some(at) = rest.find("add_executable(") {
        rest = &rest[at + "add_executable(".len()..];
        let Some(close) = rest.find(')') else { break };
        let (args, tail) = rest.split_at(close);
        rest = tail;
        for (index, token) in args.split_whitespace().enumerate() {
            let token = token.trim_matches('"');
            // The first token is the target name.
            if index == 0 || token.is_empty() {
                continue;
            }
            if KEYWORDS.contains(&token) || token.contains('$') {
                continue;
            }
            if Path::new(token)
                .extension()
                .is_none_or(|ext| ext.is_empty())
            {
                continue;
            }
            let token = token.to_string();
            if !out.contains(&token) {
                out.push(token);
            }
        }
    }
    out
}

// ── tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::content_reader::FilesystemReader;

    /// nw-351: a CMake project contributed no entry files at all, while
    /// `Cargo.toml` and `package.json` both do — so `dead_code`'s
    /// manifest-driven seeding had nothing to work with on any C++ corpus.
    #[test]
    fn parse_cmake_extracts_executable_sources_as_entry_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("CMakeLists.txt"),
            "project(demo)\n\
             find_package(Threads REQUIRED)\n\
             add_executable(demo_cli\n\
             \x20   src/main.cpp\n\
             \x20   src/cli.cpp)\n\
             add_executable(tool WIN32 tools/tool.cpp ${GENERATED_SRC})\n\
             add_library(demo_core src/core.cpp)\n",
        )
        .unwrap();
        let info = parse_manifest(&FilesystemReader::new(dir.path()));
        assert_eq!(info.package_name.as_deref(), Some("demo"));
        assert_eq!(
            info.entry_files,
            vec![
                "src/main.cpp".to_string(),
                "src/cli.cpp".to_string(),
                "tools/tool.cpp".to_string(),
            ],
            "the target NAME, CMake keywords, unresolved `${{...}}` variables \
             and every add_library source must all stay out"
        );
    }

    #[test]
    fn parse_package_json_extracts_name_and_deps() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("package.json"),
            r#"{
                "name": "@myorg/api-client",
                "dependencies": { "axios": "^1.0.0", "@myorg/shared-types": "^2.0.0" },
                "devDependencies": { "jest": "^29.0.0" }
            }"#,
        )
        .unwrap();
        let info = parse_manifest(&FilesystemReader::new(dir.path()));
        assert_eq!(info.package_name.as_deref(), Some("@myorg/api-client"));
        assert!(info.dependencies.contains(&"axios".to_string()));
        assert!(
            info.dependencies
                .contains(&"@myorg/shared-types".to_string())
        );
        assert!(info.dependencies.contains(&"jest".to_string()));
    }

    #[test]
    fn parse_go_mod_extracts_module_and_requires() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("go.mod"),
            "module github.com/myorg/service\n\ngo 1.21\n\nrequire (\n\tgithub.com/myorg/shared v1.0.0\n\tgithub.com/pkg/errors v0.9.1\n)\n",
        )
        .unwrap();
        let info = parse_manifest(&FilesystemReader::new(dir.path()));
        assert_eq!(
            info.package_name.as_deref(),
            Some("github.com/myorg/service")
        );
        assert!(
            info.dependencies
                .contains(&"github.com/myorg/shared".to_string())
        );
    }

    #[test]
    fn parse_cargo_toml_extracts_name_and_deps() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("Cargo.toml"),
            r#"
[package]
name = "my-crate"
version = "0.1.0"

[dependencies]
serde = "1.0"
my-shared = { path = "../shared" }
"#,
        )
        .unwrap();
        let info = parse_manifest(&FilesystemReader::new(dir.path()));
        assert_eq!(info.package_name.as_deref(), Some("my-crate"));
        assert!(info.dependencies.contains(&"serde".to_string()));
        assert!(info.dependencies.contains(&"my-shared".to_string()));
    }

    #[test]
    fn parse_returns_default_for_no_manifest() {
        let dir = tempfile::tempdir().unwrap();
        let info = parse_manifest(&FilesystemReader::new(dir.path()));
        assert!(info.package_name.is_none());
        assert!(info.dependencies.is_empty());
    }

    #[test]
    fn parse_pyproject_extracts_name_and_deps() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("pyproject.toml"),
            r#"
[project]
name = "myservice"
dependencies = ["requests>=2.28", "pydantic>=2.0"]
"#,
        )
        .unwrap();
        let info = parse_manifest(&FilesystemReader::new(dir.path()));
        assert_eq!(info.package_name.as_deref(), Some("myservice"));
        assert!(info.dependencies.contains(&"requests".to_string()));
        assert!(info.dependencies.contains(&"pydantic".to_string()));
    }

    #[test]
    fn parse_requirements_txt_extracts_deps() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("requirements.txt"),
            "# comment\nrequests==2.28.0\npydantic>=2.0\n-r other.txt\n",
        )
        .unwrap();
        let info = parse_manifest(&FilesystemReader::new(dir.path()));
        assert!(info.package_name.is_none());
        assert!(info.dependencies.contains(&"requests".to_string()));
        assert!(info.dependencies.contains(&"pydantic".to_string()));
    }

    #[test]
    fn save_and_load_manifest_cache_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let cache_path = dir.path().join("test.manifests.json");

        let mut cache = HashMap::new();
        cache.insert(
            "r1".to_string(),
            ManifestInfo {
                package_name: Some("my-pkg".to_string()),
                dependencies: vec!["dep-a".to_string()],
                entry_files: vec![],
            },
        );
        save_manifest_cache(&cache, &cache_path).unwrap();

        let loaded = load_manifest_cache(&cache_path).unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded["r1"].package_name.as_deref(), Some("my-pkg"));
        assert!(loaded["r1"].dependencies.contains(&"dep-a".to_string()));
    }

    #[test]
    fn save_manifest_cache_replaces_the_sidecar_inode() {
        let dir = tempfile::tempdir().unwrap();
        let cache_path = dir.path().join("brain.lbug.manifests.json");
        let old_link = dir.path().join("old-manifests.json");
        let old = HashMap::from([(
            "repo:old".to_string(),
            ManifestInfo {
                package_name: Some("old-package".to_string()),
                dependencies: Vec::new(),
                entry_files: Vec::new(),
            },
        )]);
        save_manifest_cache(&old, &cache_path).unwrap();
        std::fs::hard_link(&cache_path, &old_link).unwrap();

        let new = HashMap::from([(
            "repo:new".to_string(),
            ManifestInfo {
                package_name: Some("new-package".to_string()),
                dependencies: Vec::new(),
                entry_files: Vec::new(),
            },
        )]);
        save_manifest_cache(&new, &cache_path).unwrap();

        assert_eq!(load_manifest_cache(&cache_path).unwrap().len(), 1);
        assert!(
            load_manifest_cache(&cache_path)
                .unwrap()
                .contains_key("repo:new")
        );
        assert_eq!(load_manifest_cache(&old_link).unwrap().len(), 1);
        assert!(
            load_manifest_cache(&old_link)
                .unwrap()
                .contains_key("repo:old")
        );
    }

    #[test]
    fn manifest_atomic_replace_cleans_partial_temp_after_write_error() {
        use std::io::Write;

        let dir = tempfile::tempdir().unwrap();
        let cache_path = dir.path().join("brain.lbug.manifests.json");
        std::fs::write(&cache_path, b"previous-valid-sidecar").unwrap();

        let error = atomic_replace_file(&cache_path, |file| {
            file.write_all(b"partial replacement")?;
            Err(std::io::Error::other("injected write failure"))
        })
        .unwrap_err();

        assert!(error.to_string().contains("injected write failure"));
        assert_eq!(
            std::fs::read(&cache_path).unwrap(),
            b"previous-valid-sidecar"
        );
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    /// nw-500. `dead-code` loaded manifests with
    /// `load_manifest_cache_for_db(..).unwrap_or_default()`, which collapses
    /// two materially different states into one empty map: "no manifests
    /// indexed yet" and "the manifests are right there and unreadable". The
    /// second silently drops every manifest-declared entry file from the
    /// reachability seed, and because entry files are ROOTS the error runs in
    /// only one direction — live code lands on a list of symbols to delete.
    ///
    /// The distinction needed no new mechanism. `artifact_sidecar::load_json`
    /// already returns `Ok(None)` for an absent sidecar and `Err` for a
    /// present-but-undecodable one; the caller was throwing that away.
    #[test]
    fn a_corrupt_manifest_sidecar_is_disclosed_and_an_absent_one_is_not() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("brain.lbug");
        let store = nestweaver_store::GraphStore::create(&db_path).unwrap();

        // ABSENT — the normal state for a graph with no code repos indexed
        // yet. It must not raise an alarm, or the disclosure cries wolf on
        // every fresh database and stops being read.
        let absent = load_manifests_for_dead_code(&store, &db_path);
        assert!(
            absent.disclosure().is_none(),
            "an absent sidecar is normal and must not be disclosed, got {:?}",
            absent.disclosure()
        );
        assert!(absent.manifests.is_empty());

        // PRESENT AND VALID — also no disclosure. This is the counterweight:
        // a healthy load must add nothing at all.
        let manifests = HashMap::from([(
            "repo:canonical".to_string(),
            ManifestInfo {
                package_name: Some("pkg".to_string()),
                dependencies: vec![],
                entry_files: vec!["index.js".to_string()],
            },
        )]);
        save_manifest_cache_for_db(&manifests, &store, &db_path).unwrap();
        let healthy = load_manifests_for_dead_code(&store, &db_path);
        assert!(
            healthy.disclosure().is_none(),
            "a successful load must add no disclosure, got {:?}",
            healthy.disclosure()
        );
        assert_eq!(
            healthy.manifests["repo:canonical"].entry_files,
            vec!["index.js".to_string()],
            "and it must actually carry the payload, or the assertion above \
             is vacuous"
        );

        // PRESENT AND CORRUPT — disclosed, with the failure and the path in
        // the message so the reader can act on it.
        std::fs::write(manifest_cache_path(&db_path), b"{ not an envelope").unwrap();
        let corrupt = load_manifests_for_dead_code(&store, &db_path);
        let disclosure = corrupt
            .disclosure()
            .expect("a sidecar that exists and cannot be read must be disclosed");
        assert!(
            disclosure.contains(&manifest_cache_path(&db_path).display().to_string()),
            "the disclosure must name the file that failed: {disclosure}"
        );
        assert!(
            disclosure.contains("entry"),
            "and must say what was lost — entry files — not merely that \
             something failed: {disclosure}"
        );
        assert!(
            corrupt.manifests.is_empty(),
            "the walk still runs, degraded, rather than refusing"
        );
    }

    #[test]
    fn save_manifest_cache_for_db_retires_legacy_only_after_canonical_save() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("brain.lbug");
        let store = nestweaver_store::GraphStore::create(&db_path).unwrap();
        let legacy_path = db_path.with_extension("manifests.json");
        std::fs::write(&legacy_path, r#"{"repo:legacy":{}}"#).unwrap();
        let manifests = HashMap::from([("repo:canonical".to_string(), ManifestInfo::default())]);

        save_manifest_cache_for_db(&manifests, &store, &db_path).unwrap();

        assert!(!legacy_path.exists());
        assert!(
            load_manifest_cache_for_db(&store, &db_path)
                .unwrap()
                .contains_key("repo:canonical")
        );

        std::fs::write(&legacy_path, r#"{"repo:still-safe":{}}"#).unwrap();
        std::fs::remove_file(manifest_cache_path(&db_path)).unwrap();
        std::fs::create_dir(manifest_cache_path(&db_path)).unwrap();
        let error = save_manifest_cache_for_db(&manifests, &store, &db_path).unwrap_err();
        assert!(!error.to_string().is_empty());
        assert!(legacy_path.exists(), "failed canonical save removed legacy");
    }

    #[test]
    fn canonical_manifest_cache_rejects_legacy_foreign_and_stale_artifacts() {
        let dir = tempfile::tempdir().unwrap();
        let source_db = dir.path().join("source.lbug");
        let source = nestweaver_store::GraphStore::create(&source_db).unwrap();
        let manifests = HashMap::from([("repo:source".to_string(), ManifestInfo::default())]);
        save_manifest_cache_for_db(&manifests, &source, &source_db).unwrap();
        let source_path = manifest_cache_path(&source_db);
        let valid = std::fs::read(&source_path).unwrap();

        std::fs::write(&source_path, r#"{"repo:legacy":{}}"#).unwrap();
        let legacy = load_manifest_cache_for_db(&source, &source_db).unwrap_err();
        assert!(
            legacy.to_string().contains("run a full reindex"),
            "{legacy}"
        );

        std::fs::write(&source_path, &valid).unwrap();
        source.bump_graph_generation();
        let stale = load_manifest_cache_for_db(&source, &source_db).unwrap_err();
        assert!(
            stale.to_string().contains("stale artifact generation"),
            "{stale}"
        );

        let foreign_db = dir.path().join("foreign.lbug");
        let foreign = nestweaver_store::GraphStore::create(&foreign_db).unwrap();
        let foreign_path = manifest_cache_path(&foreign_db);
        std::fs::write(&foreign_path, valid).unwrap();
        let foreign_error = load_manifest_cache_for_db(&foreign, &foreign_db).unwrap_err();
        assert!(
            foreign_error
                .to_string()
                .contains("foreign artifact identity"),
            "{foreign_error}"
        );
    }

    #[test]
    fn committed_graph_publication_advances_once_and_reconciles_derived_artifacts() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("brain.lbug");
        let store = nestweaver_store::GraphStore::create(&db_path).unwrap();
        let manifests = HashMap::from([("repo:stable".to_string(), ManifestInfo::default())]);
        save_manifest_cache_for_db(&manifests, &store, &db_path).unwrap();
        store
            .compute_pagerank(
                0.85,
                20,
                &nestweaver_store::ranking::GraphScope::code_only(),
            )
            .unwrap();
        let pagerank_path = crate::sidecar_path(&db_path, ".pagerank.json");
        store.save_pagerank_cache(&pagerank_path).unwrap();

        let unchanged = finalize_committed_graph_mutation(&store, false);
        assert_eq!(
            unchanged.disposition,
            GraphMutationPublicationDisposition::ConfirmedNoChange
        );
        assert_eq!(unchanged.generation_before, unchanged.generation_after);
        assert!(pagerank_path.exists());

        let changed = finalize_committed_graph_mutation(&store, true);
        assert_eq!(
            changed.disposition,
            GraphMutationPublicationDisposition::CommittedComplete
        );
        assert_eq!(changed.generation_after, changed.generation_before + 1);
        assert_eq!(store.graph_generation(), changed.generation_after);
        assert_eq!(
            std::fs::read_to_string(crate::sidecar_path(&db_path, ".generation"))
                .unwrap()
                .parse::<u64>()
                .unwrap(),
            changed.generation_after
        );
        assert!(!pagerank_path.exists());
        assert!(
            load_manifest_cache_for_db(&store, &db_path)
                .unwrap()
                .contains_key("repo:stable")
        );

        let repeated_noop = finalize_committed_graph_mutation(&store, false);
        assert_eq!(repeated_noop.generation_after, changed.generation_after);
    }

    #[test]
    fn committed_graph_publication_reports_reconciliation_degradation() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("brain.lbug");
        let store = nestweaver_store::GraphStore::create(&db_path).unwrap();
        let pagerank_path = crate::sidecar_path(&db_path, ".pagerank.json");
        std::fs::create_dir(&pagerank_path).unwrap();

        let publication =
            begin_graph_mutation_publication(&store, "injected reconciliation failure").unwrap();
        let outcome = publication.finish(true).unwrap();

        assert_eq!(
            outcome.disposition,
            GraphMutationPublicationDisposition::CommittedDegraded
        );
        assert_eq!(outcome.generation_after, outcome.generation_before + 1);
        assert!(
            outcome
                .warnings
                .iter()
                .any(|warning| warning.stage == "invalidate-pagerank-sidecar")
        );
        assert!(
            crate::sidecar_path(&db_path, ".index-dirty").exists(),
            "degraded reconciliation must retain the crash fence"
        );

        let backup_config = crate::backup::BackupConfig {
            db_path: db_path.clone(),
            output_path: dir.path().join("backup.nwsnap.zst"),
            include_clones: false,
            instance_id: "test".to_string(),
            workspace_path: None,
        };
        let backup_error = crate::backup::stage_backup_from_store(&store, &backup_config)
            .err()
            .expect("backup must fail closed on committed-but-degraded publication")
            .to_string();
        assert!(
            backup_error.contains("dirty index publication"),
            "{backup_error}"
        );

        let snapshot_dir = dir.path().join("snapshot");
        let snapshot_stamp = crate::snapshot::Stamp {
            format_version: 0,
            capabilities: Vec::new(),
            instance_id: "test".to_string(),
            brain_uuid: String::new(),
            publication_uuid: String::new(),
            engine_version: env!("CARGO_PKG_VERSION").to_string(),
            min_compatible_engine: crate::snapshot::MIN_SNAPSHOT_READER_VERSION.to_string(),
            schema_hash_core: nestweaver_schema::core_schema_hash(),
            schema_hash_extensions: "none".to_string(),
            schema_hash_effective: "schema".to_string(),
            embedding_model_id: "model".to_string(),
            embedding_dimension: 0,
            embedding_count: 0,
            built_at: "2026-09-02T00:00:00Z".to_string(),
            repos: Vec::new(),
        };
        let snapshot_manifest = crate::snapshot::Manifest { repos: Vec::new() };
        let snapshot_error = crate::snapshot::build_snapshot_from_store(
            &snapshot_dir,
            &snapshot_stamp,
            &snapshot_manifest,
            &store,
        )
        .expect_err("snapshot must fail closed on committed-but-degraded publication")
        .to_string();
        assert!(
            snapshot_error.contains("dirty index publication"),
            "{snapshot_error}"
        );
    }

    #[test]
    fn interrupted_bracketed_mutation_retains_fail_closed_marker() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("brain.lbug");
        let store = nestweaver_store::GraphStore::create(&db_path).unwrap();
        let publication = begin_graph_mutation_publication(&store, "interrupted mutation").unwrap();
        store
            .insert_project(&nestweaver_schema::Project {
                uid: "proj:test:interrupted".to_string(),
                name: "interrupted".to_string(),
                summary: None,
                instance_id: "test".to_string(),
            })
            .unwrap();

        // Model interruption after the graph commit but before `finish`.
        drop(publication);

        assert!(crate::sidecar_path(&db_path, ".index-dirty").exists());
        assert_eq!(store.graph_generation(), 0);
        let error = store
            .ensure_pagerank_loaded()
            .expect_err("ranked reads must fail closed across the interruption window")
            .to_string();
        assert!(error.contains("dirty index publication"), "{error}");
    }

    #[test]
    fn load_manifest_cache_returns_empty_when_missing() {
        let dir = tempfile::tempdir().unwrap();
        let cache_path = dir.path().join("nonexistent.json");
        let loaded = load_manifest_cache(&cache_path).unwrap();
        assert!(loaded.is_empty());
    }

    #[test]
    fn parse_composer_json_extracts_name_and_deps() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("composer.json"),
            r#"{"name":"myorg/api","require":{"laravel/framework":"^10.0"},"require-dev":{"phpunit/phpunit":"^10.0"}}"#,
        )
        .unwrap();
        let info = parse_manifest(&FilesystemReader::new(dir.path()));
        assert_eq!(info.package_name.as_deref(), Some("myorg/api"));
        assert!(info.dependencies.contains(&"laravel/framework".to_string()));
        assert!(info.dependencies.contains(&"phpunit/phpunit".to_string()));
    }

    #[test]
    fn parse_gemfile_extracts_deps() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("Gemfile"),
            "source 'https://rubygems.org'\n\ngem 'rails', '~> 7.0'\ngem 'pg'\n",
        )
        .unwrap();
        let info = parse_manifest(&FilesystemReader::new(dir.path()));
        assert!(info.dependencies.contains(&"rails".to_string()));
        assert!(info.dependencies.contains(&"pg".to_string()));
    }

    #[test]
    fn parse_pubspec_yaml_extracts_name_and_deps() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("pubspec.yaml"),
            "name: my_app\ndependencies:\n  http: ^0.13.0\n",
        )
        .unwrap();
        let info = parse_manifest(&FilesystemReader::new(dir.path()));
        assert_eq!(info.package_name.as_deref(), Some("my_app"));
        assert!(info.dependencies.contains(&"http".to_string()));
    }

    #[test]
    fn parse_package_swift_extracts_deps() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("Package.swift"),
            "import PackageDescription\nlet package = Package(\n    name: \"MyPkg\",\n    dependencies: [\n        .package(url: \"https://github.com/apple/swift-argument-parser.git\", from: \"1.0.0\"),\n    ]\n)\n",
        )
        .unwrap();
        let info = parse_manifest(&FilesystemReader::new(dir.path()));
        assert_eq!(info.package_name.as_deref(), Some("MyPkg"));
        assert!(
            info.dependencies
                .contains(&"swift-argument-parser".to_string())
        );
    }

    #[test]
    fn parse_csproj_extracts_deps() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("MyApp.csproj"),
            "<Project Sdk=\"Microsoft.NET.Sdk\">\n  <ItemGroup>\n    <PackageReference Include=\"Newtonsoft.Json\" Version=\"13.0.1\" />\n  </ItemGroup>\n</Project>",
        )
        .unwrap();
        let info = parse_manifest(&FilesystemReader::new(dir.path()));
        assert!(info.dependencies.contains(&"Newtonsoft.Json".to_string()));
    }

    #[test]
    fn parse_build_gradle_kts_extracts_deps() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("build.gradle.kts"),
            "dependencies {\n    implementation(\"org.springframework.boot:spring-boot-starter-web:3.1.0\")\n}\n",
        )
        .unwrap();
        let info = parse_manifest(&FilesystemReader::new(dir.path()));
        assert!(
            info.dependencies
                .contains(&"org.springframework.boot:spring-boot-starter-web".to_string())
        );
    }

    #[test]
    fn parse_cmake_extracts_name_and_deps() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("CMakeLists.txt"),
            "cmake_minimum_required(VERSION 3.20)\nproject(MyApp)\nfind_package(Boost REQUIRED)\nfind_package(OpenSSL REQUIRED)\n",
        )
        .unwrap();
        let info = parse_manifest(&FilesystemReader::new(dir.path()));
        assert_eq!(info.package_name.as_deref(), Some("MyApp"));
        assert!(info.dependencies.contains(&"Boost".to_string()));
        assert!(info.dependencies.contains(&"OpenSSL".to_string()));
    }

    #[test]
    fn parse_package_json_extracts_entry_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("package.json"),
            r#"{
                "name": "my-lib",
                "main": "./dist/index.js",
                "bin": {
                    "cli": "./bin/cli.js"
                },
                "exports": {
                    ".": {
                        "import": "./dist/esm/index.js",
                        "require": "./dist/cjs/index.js"
                    },
                    "./utils": "./dist/utils.js"
                },
                "dependencies": {}
            }"#,
        )
        .unwrap();
        let info = parse_manifest(&FilesystemReader::new(dir.path()));
        // Root-relative entries are rebased/normalized like every other
        // package.json's, so the leading `./` is dropped (parent() of the
        // root "package.json" is "", which is an identity join).
        assert!(info.entry_files.contains(&"dist/index.js".to_string()));
        assert!(info.entry_files.contains(&"bin/cli.js".to_string()));
        assert!(info.entry_files.contains(&"dist/esm/index.js".to_string()));
        assert!(info.entry_files.contains(&"dist/cjs/index.js".to_string()));
        assert!(info.entry_files.contains(&"dist/utils.js".to_string()));
    }

    #[test]
    fn parse_package_json_bin_as_string() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("package.json"),
            r#"{
                "name": "my-cli",
                "bin": "./bin/main.js",
                "dependencies": {}
            }"#,
        )
        .unwrap();
        let info = parse_manifest(&FilesystemReader::new(dir.path()));
        assert!(info.entry_files.contains(&"bin/main.js".to_string()));
    }

    #[test]
    fn parse_package_json_no_entry_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("package.json"),
            r#"{
                "name": "simple-pkg",
                "dependencies": { "lodash": "^4.0.0" }
            }"#,
        )
        .unwrap();
        let info = parse_manifest(&FilesystemReader::new(dir.path()));
        assert!(info.entry_files.is_empty());
    }

    // ── nw-492 (Task 2.7A): nested package.json entry-point discovery ──────

    /// The witness shape: a repo whose root manifest is NOT package.json
    /// (mirrors this very repo's own root Cargo.toml) with a nested
    /// wasm-bindgen-style glue package deeper in the tree. `main` must come
    /// back rebased onto the nested manifest's own directory, not the bare
    /// literal string package.json wrote.
    #[test]
    fn nested_package_json_main_is_rebased_onto_its_own_directory() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("Cargo.toml"),
            "[package]\nname = \"host-crate\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        std::fs::create_dir_all(dir.path().join("crates/x/pkg")).unwrap();
        std::fs::write(
            dir.path().join("crates/x/pkg/package.json"),
            r#"{"name": "glue", "main": "./a.js"}"#,
        )
        .unwrap();

        let info = parse_manifest(&FilesystemReader::new(dir.path()));
        assert_eq!(info.package_name.as_deref(), Some("host-crate"));
        assert!(
            info.entry_files.contains(&"crates/x/pkg/a.js".to_string()),
            "{:?}",
            info.entry_files
        );
        assert!(
            !info.entry_files.contains(&"a.js".to_string()),
            "the unrebased literal must not appear: {:?}",
            info.entry_files
        );
    }

    #[test]
    fn root_package_json_with_name_yields_identical_entries_to_nameless_root() {
        let named_dir = tempfile::tempdir().unwrap();
        std::fs::write(
            named_dir.path().join("package.json"),
            r#"{"name": "has-a-name", "main": "./index.js"}"#,
        )
        .unwrap();
        let named = parse_manifest(&FilesystemReader::new(named_dir.path()));

        let nameless_dir = tempfile::tempdir().unwrap();
        std::fs::write(
            nameless_dir.path().join("package.json"),
            r#"{"main": "./index.js"}"#,
        )
        .unwrap();
        let nameless = parse_manifest(&FilesystemReader::new(nameless_dir.path()));

        assert_eq!(named.package_name.as_deref(), Some("has-a-name"));
        assert_eq!(nameless.package_name, None);
        assert_eq!(
            named.entry_files, nameless.entry_files,
            "entry-file discovery must not depend on the `name` field"
        );
        assert_eq!(named.entry_files, vec!["index.js".to_string()]);
    }

    #[test]
    fn nameless_root_package_json_still_contributes_entry_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("package.json"),
            r#"{"main": "./index.js", "bin": "./cli.js"}"#,
        )
        .unwrap();
        let info = parse_manifest(&FilesystemReader::new(dir.path()));
        assert!(info.package_name.is_none());
        assert!(info.entry_files.contains(&"index.js".to_string()));
        assert!(info.entry_files.contains(&"cli.js".to_string()));
    }

    #[test]
    fn exports_string_form_is_collected() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("package.json"),
            r#"{"name": "x", "exports": "./index.js"}"#,
        )
        .unwrap();
        let info = parse_manifest(&FilesystemReader::new(dir.path()));
        assert!(info.entry_files.contains(&"index.js".to_string()));
    }

    #[test]
    fn exports_conditions_map_is_collected() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("package.json"),
            r#"{
                "name": "x",
                "exports": {
                    ".": { "import": "./esm.js", "require": "./cjs.js" }
                }
            }"#,
        )
        .unwrap();
        let info = parse_manifest(&FilesystemReader::new(dir.path()));
        assert!(info.entry_files.contains(&"esm.js".to_string()));
        assert!(info.entry_files.contains(&"cjs.js".to_string()));
    }

    #[test]
    fn browser_string_is_included_but_object_form_is_ignored() {
        let string_dir = tempfile::tempdir().unwrap();
        std::fs::write(
            string_dir.path().join("package.json"),
            r#"{"name": "x", "browser": "./web.js"}"#,
        )
        .unwrap();
        let string_info = parse_manifest(&FilesystemReader::new(string_dir.path()));
        assert!(string_info.entry_files.contains(&"web.js".to_string()));

        let object_dir = tempfile::tempdir().unwrap();
        std::fs::write(
            object_dir.path().join("package.json"),
            r#"{"name": "x", "browser": {"./server.js": "./client.js"}}"#,
        )
        .unwrap();
        let object_info = parse_manifest(&FilesystemReader::new(object_dir.path()));
        assert!(
            !object_info.entry_files.contains(&"client.js".to_string()),
            "{:?}",
            object_info.entry_files
        );
        assert!(
            !object_info.entry_files.contains(&"server.js".to_string()),
            "{:?}",
            object_info.entry_files
        );
    }

    #[test]
    fn rebase_entry_normalizes_dot_dot_within_the_repo() {
        // "crates/x/pkg/package.json" declaring "../shared/util.js" should
        // land at "crates/x/shared/util.js" — one `..` pops the manifest's
        // own directory, not the repo root.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("Cargo.toml"),
            "[package]\nname = \"host\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        std::fs::create_dir_all(dir.path().join("crates/x/pkg")).unwrap();
        std::fs::write(
            dir.path().join("crates/x/pkg/package.json"),
            r#"{"name": "glue", "main": "../shared/util.js"}"#,
        )
        .unwrap();
        let info = parse_manifest(&FilesystemReader::new(dir.path()));
        assert!(
            info.entry_files
                .contains(&"crates/x/shared/util.js".to_string()),
            "{:?}",
            info.entry_files
        );
    }

    #[test]
    fn rebase_entry_dropped_when_dot_dot_escapes_the_repo_root() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("Cargo.toml"),
            "[package]\nname = \"host\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        std::fs::create_dir_all(dir.path().join("crates/x")).unwrap();
        std::fs::write(
            dir.path().join("crates/x/package.json"),
            r#"{"name": "glue", "main": "../../../etc/passwd"}"#,
        )
        .unwrap();
        let info = parse_manifest(&FilesystemReader::new(dir.path()));
        assert!(
            info.entry_files.is_empty(),
            "an entry that walks above the repo root must be dropped: {:?}",
            info.entry_files
        );
    }

    #[test]
    fn star_pattern_export_target_is_dropped() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("package.json"),
            r#"{
                "name": "x",
                "main": "./index.js",
                "exports": { "./*": "./src/*.js" }
            }"#,
        )
        .unwrap();
        let info = parse_manifest(&FilesystemReader::new(dir.path()));
        assert!(info.entry_files.contains(&"index.js".to_string()));
        assert!(
            !info.entry_files.iter().any(|e| e.contains('*')),
            "{:?}",
            info.entry_files
        );
    }

    #[test]
    fn node_modules_package_json_is_ignored() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("Cargo.toml"),
            "[package]\nname = \"host\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        std::fs::create_dir_all(dir.path().join("node_modules/some-dep")).unwrap();
        std::fs::write(
            dir.path().join("node_modules/some-dep/package.json"),
            r#"{"name": "some-dep", "main": "./index.js"}"#,
        )
        .unwrap();
        let info = parse_manifest(&FilesystemReader::new(dir.path()));
        assert!(
            info.entry_files.is_empty(),
            "a node_modules package.json must not contribute entry files: {:?}",
            info.entry_files
        );
    }

    /// Counterweight: `suggest_links` (`suggest.rs`) reads only
    /// `package_name`/`dependencies` off `ManifestInfo`. This pins that a
    /// root `Cargo.toml`'s name/dependencies are unaffected by entry-file
    /// discovery, and that no package.json anywhere means no entry files.
    #[test]
    fn root_cargo_toml_keeps_its_name_and_dependencies_with_no_entry_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("Cargo.toml"),
            "[package]\nname = \"my-crate\"\nversion = \"0.1.0\"\n\n[dependencies]\nserde = \"1.0\"\n",
        )
        .unwrap();
        let info = parse_manifest(&FilesystemReader::new(dir.path()));
        assert_eq!(info.package_name.as_deref(), Some("my-crate"));
        assert!(info.dependencies.contains(&"serde".to_string()));
        assert!(info.entry_files.is_empty());
    }

    /// Counterweight: a root `package.json`'s `package_name`/`dependencies`
    /// are unaffected by folding entry-file discovery into `parse_manifest`.
    #[test]
    fn root_package_json_keeps_its_name_and_dependencies() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("package.json"),
            r#"{
                "name": "@myorg/api-client",
                "dependencies": { "axios": "^1.0.0" }
            }"#,
        )
        .unwrap();
        let info = parse_manifest(&FilesystemReader::new(dir.path()));
        assert_eq!(info.package_name.as_deref(), Some("@myorg/api-client"));
        assert!(info.dependencies.contains(&"axios".to_string()));
    }

    // ── 2.7A review follow-up: absolute/array/directory/empty/backslash ────

    /// IMPORTANT (review): an absolute path must never be silently rebased
    /// under a nested manifest's own directory — that would produce a
    /// plausible-looking but wrong repo-relative path instead of being
    /// dropped.
    #[test]
    fn absolute_entry_is_dropped() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("Cargo.toml"),
            "[package]\nname = \"host\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        std::fs::create_dir_all(dir.path().join("crates/x")).unwrap();
        std::fs::write(
            dir.path().join("crates/x/package.json"),
            r#"{"name": "glue", "main": "/dist/index.js"}"#,
        )
        .unwrap();
        let info = parse_manifest(&FilesystemReader::new(dir.path()));
        assert!(
            info.entry_files.is_empty(),
            "an absolute path must be dropped, not rebased under the \
             manifest's directory: {:?}",
            info.entry_files
        );
    }

    /// `exports` may be Node's documented array fallback list; every string
    /// target in it is a candidate entry point.
    #[test]
    fn exports_array_form_is_collected() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("package.json"),
            r#"{"name": "x", "exports": ["./modern.js", "./legacy.js"]}"#,
        )
        .unwrap();
        let info = parse_manifest(&FilesystemReader::new(dir.path()));
        assert!(info.entry_files.contains(&"modern.js".to_string()));
        assert!(info.entry_files.contains(&"legacy.js".to_string()));
    }

    #[test]
    fn trailing_slash_directory_entry_is_dropped() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("package.json"),
            r#"{"name": "x", "main": "lib/"}"#,
        )
        .unwrap();
        let info = parse_manifest(&FilesystemReader::new(dir.path()));
        assert!(
            info.entry_files.is_empty(),
            "a directory-main target must not become the inert literal \
             \"lib\" — npm's directory-main resolution is unimplemented \
             (nw-499), so it must be dropped, not mis-rooted: {:?}",
            info.entry_files
        );
    }

    #[test]
    fn empty_string_entry_is_dropped() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("Cargo.toml"),
            "[package]\nname = \"host\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        std::fs::create_dir_all(dir.path().join("crates/x")).unwrap();
        std::fs::write(
            dir.path().join("crates/x/package.json"),
            r#"{"name": "glue", "main": "   "}"#,
        )
        .unwrap();
        let info = parse_manifest(&FilesystemReader::new(dir.path()));
        assert!(
            info.entry_files.is_empty(),
            "a blank entry must not rebase to the manifest's own directory: {:?}",
            info.entry_files
        );
    }

    #[test]
    fn backslash_and_url_entries_are_dropped() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("package.json"),
            r#"{
                "name": "x",
                "main": "./ok.js",
                "bin": {
                    "win": "src\\win.js",
                    "remote": "https://example.com/cli.js"
                }
            }"#,
        )
        .unwrap();
        let info = parse_manifest(&FilesystemReader::new(dir.path()));
        assert!(info.entry_files.contains(&"ok.js".to_string()));
        assert!(
            !info
                .entry_files
                .iter()
                .any(|e| e.contains('\\') || e.contains("example.com")),
            "{:?}",
            info.entry_files
        );
    }
}

#[cfg(test)]
mod hardening_embedding_recovery_tests {
    use super::*;
    #[test]
    fn failed_embedding_reconciliation_keeps_durable_recovery_fence() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("brain.lbug");
        let store = nestweaver_store::GraphStore::create(&db).unwrap();
        store.set_embedding_metadata("fixture", 2).unwrap();
        // A vector whose graph node was deleted: occupancy alone calls it live.
        assert!(store.add_embedding("sym:deleted", vec![1.0, 0.0]));
        store.flush_embedding_index().unwrap();
        assert_eq!(store.embedding_index_occupancy().tombstoned, 0);
        let journal = crate::sidecar_path(&db, ".embeddings.journal");
        std::fs::create_dir(&journal).unwrap();
        let guard = begin_graph_mutation_publication(&store, "post-delete regression").unwrap();
        let outcome = guard.finish(true).unwrap();
        assert!(outcome.is_degraded());
        assert!(
            outcome
                .warnings
                .iter()
                .any(|w| w.stage == "embedding-index")
        );
        assert!(crate::sidecar_path(&db, ".index-dirty").exists());
        drop(store);
        // The durable fence survives reopening before repair; ranked queries
        // cannot treat the old artifact as a clean publication.
        if let Ok(unrepaired) = nestweaver_store::GraphStore::open_read_only(&db) {
            assert!(unrepaired.is_index_publication_dirty());
        }
        std::fs::remove_dir(&journal).unwrap();
        let authority = nestweaver_store::acquire_db_write_lease(&db).unwrap();
        let store = nestweaver_store::GraphStore::open_with_authority(&db, &authority).unwrap();
        crate::index::force_recover_index_publication(&store, &authority).unwrap();
        assert!(!store.has_embedding("sym:deleted"));
        assert!(!crate::sidecar_path(&db, ".index-dirty").exists());
        drop(store);
        drop(authority);
        let reopened = nestweaver_store::GraphStore::open_read_only(&db).unwrap();
        assert!(!reopened.has_embedding("sym:deleted"));
    }
}

#[cfg(test)]
mod release_manifest_tests {
    use super::*;

    #[test]
    fn captured_manifest_inputs_distinguish_absent_malformed_and_changed_sources() {
        let dir = tempfile::tempdir().unwrap();
        let reader = crate::content_reader::FilesystemReader::new(dir.path()).strict_enumeration();
        let capture = || {
            capture_manifest_inputs(
                &reader,
                &mut (64 * 1024 * 1024),
                std::time::Instant::now() + std::time::Duration::from_secs(60),
            )
        };
        let empty = capture().unwrap();
        assert!(parse_manifest(&empty).package_name.is_none());
        std::fs::write(dir.path().join("package.json"), "{").unwrap();
        assert!(
            capture().is_err(),
            "malformed JSON must not become an empty manifest"
        );
        std::fs::write(dir.path().join("package.json"), r#"{"name":"fixed"}"#).unwrap();
        let fixed = capture().unwrap();
        assert_ne!(fixed.digest(), empty.digest());
        assert_eq!(
            parse_manifest(&fixed).package_name.as_deref(),
            Some("fixed")
        );
        assert!(
            capture_manifest_inputs(
                &reader,
                &mut 1,
                std::time::Instant::now() + std::time::Duration::from_secs(60)
            )
            .is_err()
        );
        assert!(capture_manifest_inputs(&reader, &mut 1000, std::time::Instant::now()).is_err());
    }

    #[test]
    fn manifest_debt_rejects_oversized_or_malformed_records() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("graph.lbug");
        mark_manifest_reconciliation_pending(&db, "fixture").unwrap();
        assert!(manifest_debt_revision(&db).unwrap().is_some());
        for bytes in [vec![b'x'; 8193], b"{}".to_vec(), b"not JSON".to_vec()] {
            std::fs::write(manifest_debt_path(&db), &bytes).unwrap();
            assert!(manifest_debt_revision(&db).is_err());
            assert_eq!(std::fs::read(manifest_debt_path(&db)).unwrap(), bytes);
        }
    }

    #[test]
    fn current_manifest_requires_exact_inventory_and_no_pending_source_debt() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("graph.lbug");
        let store = nestweaver_store::GraphStore::create(&db).unwrap();
        assert!(current_manifest_snapshot(&store, &db).unwrap().is_empty());
        mark_manifest_reconciliation_pending(&db, "initial index failed before repo insertion")
            .unwrap();
        assert_eq!(
            current_manifest_snapshot(&store, &db).unwrap_err().reason,
            ManifestUnavailableReason::PendingSourceChange
        );
        std::fs::remove_file(manifest_debt_path(&db)).unwrap();
        for uid in ["repo:a", "repo:b"] {
            store
                .insert_repo(&nestweaver_schema::Repo {
                    uid: uid.into(),
                    url: format!("file:///fixture/{uid}"),
                    indexed_sha: "sha".into(),
                    staleness_commits_behind: 0,
                    instance_id: "fixture".into(),
                    name: None,
                    root_path: None,
                })
                .unwrap();
        }
        assert_eq!(
            current_manifest_snapshot(&store, &db).unwrap_err().reason,
            ManifestUnavailableReason::Missing
        );
        let mut map = HashMap::from([("repo:a".into(), ManifestInfo::default())]);
        save_manifest_cache_for_db(&map, &store, &db).unwrap();
        assert_eq!(
            current_manifest_snapshot(&store, &db).unwrap_err().reason,
            ManifestUnavailableReason::IncompleteCoverage
        );
        map.insert("repo:b".into(), ManifestInfo::default());
        save_manifest_cache_for_db(&map, &store, &db).unwrap();
        assert_eq!(current_manifest_snapshot(&store, &db).unwrap().len(), 2);
        mark_manifest_reconciliation_pending(&db, "source edit").unwrap();
        assert_eq!(
            current_manifest_snapshot(&store, &db).unwrap_err().reason,
            ManifestUnavailableReason::PendingSourceChange
        );
        let before = std::fs::read(manifest_cache_path(&db)).unwrap();
        store.bump_graph_generation();
        let error = current_manifest_snapshot(&store, &db).unwrap_err();
        assert_eq!(error.reason, ManifestUnavailableReason::StaleGeneration);
        assert_eq!(error.expected_generation, store.graph_generation());
        assert_eq!(std::fs::read(manifest_cache_path(&db)).unwrap(), before);
    }
}
