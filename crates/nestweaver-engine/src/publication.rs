//! Canonical publication-root layout and durable `CURRENT` selection.
//!
//! The graph and its derived artifacts live in immutable publication slots.
//! Selecting a slot is a small compare-and-swap operation over a checksummed
//! pointer, never a destructive rename of the incumbent database.

use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::{Path, PathBuf};

pub const CURRENT_POINTER_VERSION: u32 = 1;
pub const PUBLICATION_MANIFEST_FILE: &str = "publication.json";
pub const PUBLICATION_GRAPH_FILE: &str = "graph.lbug";
pub const SOURCE_MANIFEST_SUFFIX: &str = ".sources.json";
pub const SOURCE_MANIFEST_ARTIFACT_KIND: &str = "publication_source_manifest";
pub const SOURCE_MANIFEST_SCHEMA_VERSION: u32 = 1;
pub const SOURCE_MANIFEST_ALGORITHM_FINGERPRINT: &str = "nestweaver-publication-source-manifest-v1";
pub const PRESERVED_STATE_SUFFIX: &str = ".preserved-state.json";
pub const PRESERVED_STATE_ARTIFACT_KIND: &str = "publication_preserved_state_receipt";
pub const PRESERVED_STATE_SCHEMA_VERSION: u32 = 1;
pub const PRESERVED_STATE_ALGORITHM_FINGERPRINT: &str = "nestweaver-publication-preserved-state-v1";

/// Validate a live PageRank envelope before a sealed publication describes
/// it, returning the exact schema and algorithm/scope fingerprint carried by
/// the payload.
///
/// Identity, producer version and source generation ARE checked against the
/// caller's expectations, so a foreign, stale or corrupt sidecar is rejected.
///
/// The algorithm fingerprint is now VERIFIED rather than trusted. This function
/// still has no independent expectation — its job is to discover what the
/// payload declares — but nw-147 made the artifact declare the parameters that
/// produced its fingerprint, so the fingerprint can be RECOMPUTED from them.
/// A same-brain, same-generation artifact computed with different
/// damping/iterations/scope is now rejected: either its declaration disagrees
/// with its fingerprint, or its declaration is honest and the recomputation
/// exposes the mismatch. An artifact can no longer vouch for its own
/// provenance.
///
/// An artifact that declares NOTHING is refused outright. Accepting it would
/// restore exactly the self-comparison this closes, and — migration being
/// explicitly on the table for 8.0.0 — a re-index is the honest remedy rather
/// than a permanent hole kept open for old sidecars.
pub(crate) fn pagerank_artifact_contract(
    bytes: &[u8],
    identity: &nestweaver_store::PublicationIdentity,
    producer_version: &str,
    source_graph_generation: u64,
) -> anyhow::Result<(u32, String)> {
    let envelope: nestweaver_store::artifact_envelope::ArtifactEnvelope =
        serde_json::from_slice(bytes).map_err(|error| {
            anyhow::anyhow!(
                "PageRank sidecar is not a self-describing v2 artifact envelope: {error}"
            )
        })?;
    if !envelope
        .algorithm_fingerprint
        .starts_with(nestweaver_store::ranking::PAGERANK_ALGORITHM_FINGERPRINT_PREFIX)
    {
        anyhow::bail!(
            "incompatible PageRank algorithm fingerprint '{}'",
            envelope.algorithm_fingerprint
        );
    }
    // nw-147: recompute from the DECLARED parameters and require agreement.
    // This is what makes the comparison below meaningful — without it, the
    // fingerprint was only ever compared to itself.
    let declared = &envelope.algorithm_parameters;
    if declared.is_null() {
        anyhow::bail!(
            "PageRank sidecar declares no algorithm parameters, so its fingerprint \
             cannot be verified and would only be compared against itself; \
             re-index to produce a self-describing artifact"
        );
    }
    // The declaration must match what THIS BUILD computes with. Recomputing
    // the fingerprint from the declaration (below) only proves the artifact
    // agrees with itself; without this an artifact declaring damping 0.5 and
    // carrying the correct fingerprint FOR 0.5 passed, because nothing
    // supplied an independent expectation. Scope is not checked here — it
    // legitimately varies between a code-only and a unified pass.
    for (field, expected, actual) in [
        (
            "damping",
            nestweaver_store::ranking::PAGERANK_DAMPING,
            declared.get("damping").and_then(|value| value.as_f64()),
        ),
        (
            "iterations",
            f64::from(nestweaver_store::ranking::PAGERANK_ITERATIONS),
            declared.get("iterations").and_then(|value| value.as_f64()),
        ),
    ] {
        let Some(actual) = actual else {
            anyhow::bail!(
                "PageRank sidecar declares no {field}, so its parameters cannot be \
                 checked against this build's"
            );
        };
        if actual != expected {
            anyhow::bail!(
                "PageRank sidecar declares {field} {actual} but this build computes \
                 with {expected}; the artifact was produced by a different \
                 configuration and its scores are not comparable"
            );
        }
    }
    let recomputed = nestweaver_store::ranking::pagerank_fingerprint_from_declared(declared)
        .ok_or_else(|| {
            anyhow::anyhow!("PageRank sidecar declares malformed algorithm parameters: {declared}")
        })?;
    if recomputed != envelope.algorithm_fingerprint {
        anyhow::bail!(
            "PageRank sidecar's fingerprint does not match the parameters it declares \
             (declared parameters produce '{recomputed}', artifact carries '{}'); \
             the artifact is mislabelled",
            envelope.algorithm_fingerprint
        );
    }
    let fingerprint = envelope.algorithm_fingerprint.clone();
    let _: std::collections::HashMap<String, f64> =
        envelope.validate_and_decode(nestweaver_store::artifact_envelope::ArtifactExpectation {
            artifact_kind: nestweaver_store::ranking::PAGERANK_ARTIFACT_KIND,
            artifact_schema_version: nestweaver_store::ranking::PAGERANK_ARTIFACT_SCHEMA_VERSION,
            identity,
            producer_version,
            source_graph_generation,
            algorithm_fingerprint: &fingerprint,
        })?;
    Ok((
        nestweaver_store::ranking::PAGERANK_ARTIFACT_SCHEMA_VERSION,
        fingerprint,
    ))
}

pub(crate) fn repo_manifest_artifact_contract(
    bytes: &[u8],
    identity: &nestweaver_store::PublicationIdentity,
    producer_version: &str,
    source_graph_generation: u64,
) -> anyhow::Result<(u32, String)> {
    let envelope: nestweaver_store::artifact_envelope::ArtifactEnvelope =
        serde_json::from_slice(bytes).map_err(|error| {
            anyhow::anyhow!(
                "repository manifest sidecar is not a self-describing v2 artifact envelope: {error}"
            )
        })?;
    let _: std::collections::HashMap<String, crate::manifest::ManifestInfo> = envelope
        .validate_and_decode(nestweaver_store::artifact_envelope::ArtifactExpectation {
            artifact_kind: crate::manifest::MANIFEST_ARTIFACT_KIND,
            artifact_schema_version: crate::manifest::MANIFEST_ARTIFACT_SCHEMA_VERSION,
            identity,
            producer_version,
            source_graph_generation,
            algorithm_fingerprint: crate::manifest::MANIFEST_ALGORITHM_FINGERPRINT,
        })?;
    Ok((
        crate::manifest::MANIFEST_ARTIFACT_SCHEMA_VERSION,
        crate::manifest::MANIFEST_ALGORITHM_FINGERPRINT.to_string(),
    ))
}

pub(crate) fn source_manifest_artifact_contract(
    bytes: &[u8],
    identity: &nestweaver_store::PublicationIdentity,
    producer_version: &str,
    source_graph_generation: u64,
) -> anyhow::Result<(u32, String)> {
    let envelope: nestweaver_store::artifact_envelope::ArtifactEnvelope =
        serde_json::from_slice(bytes).map_err(|error| {
            anyhow::anyhow!("source manifest is not a self-describing artifact: {error}")
        })?;
    let _: serde_json::Value =
        envelope.validate_and_decode(nestweaver_store::artifact_envelope::ArtifactExpectation {
            artifact_kind: SOURCE_MANIFEST_ARTIFACT_KIND,
            artifact_schema_version: SOURCE_MANIFEST_SCHEMA_VERSION,
            identity,
            producer_version,
            source_graph_generation,
            algorithm_fingerprint: SOURCE_MANIFEST_ALGORITHM_FINGERPRINT,
        })?;
    Ok((
        SOURCE_MANIFEST_SCHEMA_VERSION,
        SOURCE_MANIFEST_ALGORITHM_FINGERPRINT.to_string(),
    ))
}

pub(crate) fn preserved_state_artifact_contract(
    bytes: &[u8],
    identity: &nestweaver_store::PublicationIdentity,
    producer_version: &str,
    source_graph_generation: u64,
) -> anyhow::Result<(u32, String)> {
    let envelope: nestweaver_store::artifact_envelope::ArtifactEnvelope =
        serde_json::from_slice(bytes).map_err(|error| {
            anyhow::anyhow!("preserved-state receipt is not self-describing: {error}")
        })?;
    let _: crate::publication_state::PreservedStateReceipt =
        envelope.validate_and_decode(nestweaver_store::artifact_envelope::ArtifactExpectation {
            artifact_kind: PRESERVED_STATE_ARTIFACT_KIND,
            artifact_schema_version: PRESERVED_STATE_SCHEMA_VERSION,
            identity,
            producer_version,
            source_graph_generation,
            algorithm_fingerprint: PRESERVED_STATE_ALGORITHM_FINGERPRINT,
        })?;
    Ok((
        PRESERVED_STATE_SCHEMA_VERSION,
        PRESERVED_STATE_ALGORITHM_FINGERPRINT.to_string(),
    ))
}

/// Typed role of one file in a sealed publication bundle.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactKind {
    Graph,
    SourceManifest,
    PreservedState,
    CompatibilityStamp,
    InstanceConfig,
    Ranking,
    RepoManifest,
    Bm25,
    Regex,
    Embeddings,
    ParsedCache,
    ResolutionDependencies,
    FileMetadata,
    GitActivity,
    Cochange,
    Interactions,
    Extensions,
    Aliases,
    Bundles,
    Generation,
    WriteAheadLog,
    WorkspaceClone,
}

/// One checksummed payload in a `PublicationBundleV3`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactDescriptor {
    pub path: String,
    pub kind: ArtifactKind,
    pub artifact_schema_version: u32,
    pub byte_size: u64,
    pub blake3: String,
    pub brain_uuid: String,
    pub publication_uuid: String,
    pub producer_version: String,
    pub source_graph_generation: u64,
    pub algorithm_fingerprint: String,
}

/// Canonical typed inventory shared by snapshot, backup, restore, and
/// publication cutover. Format-specific compatibility projections may coexist
/// in a bundle, but trust is rooted in this exact inventory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PublicationBundleV3 {
    pub format_version: u32,
    pub brain_uuid: String,
    pub publication_uuid: String,
    pub producer_version: String,
    pub source_graph_generation: u64,
    pub artifacts: Vec<ArtifactDescriptor>,
}

impl PublicationBundleV3 {
    /// Validate metadata common to every sealed representation. Format-specific
    /// readers additionally verify exact path coverage and payload bytes.
    pub fn validate_metadata(&self, expected_format_version: u32) -> anyhow::Result<()> {
        if self.format_version != expected_format_version {
            anyhow::bail!(
                "publication bundle format {} does not match expected v{expected_format_version}",
                self.format_version
            );
        }
        let identity = nestweaver_store::PublicationIdentity {
            brain_uuid: self.brain_uuid.clone(),
            publication_uuid: self.publication_uuid.clone(),
        };
        identity
            .validate()
            .map_err(|error| anyhow::anyhow!("invalid publication bundle identity: {error}"))?;
        if self.producer_version.is_empty() {
            anyhow::bail!("publication bundle producer version is empty");
        }
        let expected_brain = parse_uuid("bundle brain_uuid", &self.brain_uuid)?;
        let expected_publication = parse_uuid("bundle publication_uuid", &self.publication_uuid)?;
        // nw-149: a manifest describing NOTHING was accepted here and only
        // caught later by `resolve_selected_database` — i.e. after the pointer
        // had already moved. A publication with no artifacts cannot be valid,
        // and the cheapest place to say so is before anything is trusted.
        if self.artifacts.is_empty() {
            anyhow::bail!("publication bundle describes no artifacts");
        }
        let mut paths = std::collections::BTreeSet::new();
        let mut case_folded = std::collections::BTreeSet::new();
        for descriptor in &self.artifacts {
            let path = Path::new(&descriptor.path);
            // nw-149: the EMPTY path was accepted here while
            // `validate_relative_path` in publication_operation.rs rejects it —
            // two validators in one subsystem disagreeing about the same
            // string. `Path::new("")` is not absolute and yields no components,
            // so both guards below pass it by construction.
            if descriptor.path.is_empty() {
                anyhow::bail!("publication bundle contains an empty artifact path");
            }
            if path.is_absolute()
                || path
                    .components()
                    .any(|component| !matches!(component, std::path::Component::Normal(_)))
            {
                anyhow::bail!(
                    "publication bundle contains unsafe artifact path: {}",
                    descriptor.path
                );
            }
            // nw-149: dedupe on the NORMALIZED path, not the raw string.
            // `a/b`, `a//b` and `a/b/` are distinct strings that name one file,
            // so a raw-string check let a manifest describe the same artifact
            // more than once with different metadata for each.
            let normalized: PathBuf = path.components().collect();
            let normalized = normalized.to_string_lossy().into_owned();
            if !paths.insert(normalized.clone()) {
                anyhow::bail!(
                    "publication bundle contains duplicate artifact: {}",
                    descriptor.path
                );
            }
            // And on a case-insensitive filesystem `graph.lbug` and
            // `GRAPH.LBUG` are ALSO one file. A manifest must not depend on
            // case to tell two artifacts apart, because whether that works is a
            // property of the reader's filesystem, not of the publication.
            if !case_folded.insert(normalized.to_lowercase()) {
                anyhow::bail!(
                    "publication bundle contains artifact paths differing only by case: {}; \
                     these name one file on a case-insensitive filesystem",
                    descriptor.path
                );
            }
            let observed_brain = parse_uuid("artifact brain_uuid", &descriptor.brain_uuid)?;
            let observed_publication =
                parse_uuid("artifact publication_uuid", &descriptor.publication_uuid)?;
            if observed_brain != expected_brain || observed_publication != expected_publication {
                anyhow::bail!(
                    "publication artifact {} has foreign identity {}/{}",
                    descriptor.path,
                    descriptor.brain_uuid,
                    descriptor.publication_uuid
                );
            }
            if descriptor.producer_version != self.producer_version
                || descriptor.algorithm_fingerprint.is_empty()
                || descriptor.artifact_schema_version == 0
            {
                anyhow::bail!(
                    "publication artifact {} has incompatible schema/producer/fingerprint metadata (artifact producer '{}', bundle producer '{}')",
                    descriptor.path,
                    descriptor.producer_version,
                    self.producer_version
                );
            }
            if descriptor.source_graph_generation != self.source_graph_generation {
                anyhow::bail!(
                    "publication artifact {} source generation {} does not match bundle {}",
                    descriptor.path,
                    descriptor.source_graph_generation,
                    self.source_graph_generation
                );
            }
            validate_digest("artifact blake3", &descriptor.blake3)?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtifactExpectation {
    pub path: String,
    pub kind: ArtifactKind,
    pub artifact_schema_version: u32,
    pub brain_uuid: String,
    pub publication_uuid: String,
    pub source_graph_generation: u64,
    pub algorithm_fingerprint: String,
}

/// Trust state for a derived artifact. Callers can safely distinguish a
/// rebuildable absence/staleness from an incompatible or foreign artifact;
/// none of these states collapse into a false `Ready` result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ArtifactState {
    Ready,
    Missing {
        path: String,
    },
    Stale {
        path: String,
        expected_generation: u64,
        observed_generation: u64,
    },
    Incompatible {
        path: String,
        reason: String,
    },
    ForeignIdentity {
        path: String,
        expected_brain_uuid: String,
        observed_brain_uuid: String,
        expected_publication_uuid: String,
        observed_publication_uuid: String,
    },
    Corrupt {
        path: String,
        reason: String,
    },
}

pub fn classify_artifact_descriptor(
    expected: &ArtifactExpectation,
    observed: Result<Option<&ArtifactDescriptor>, String>,
) -> ArtifactState {
    let descriptor = match observed {
        Ok(Some(descriptor)) => descriptor,
        Ok(None) => {
            return ArtifactState::Missing {
                path: expected.path.clone(),
            };
        }
        Err(reason) => {
            return ArtifactState::Corrupt {
                path: expected.path.clone(),
                reason,
            };
        }
    };
    let observed_identity = nestweaver_store::PublicationIdentity {
        brain_uuid: descriptor.brain_uuid.clone(),
        publication_uuid: descriptor.publication_uuid.clone(),
    };
    if let Err(error) = observed_identity.validate() {
        return ArtifactState::Corrupt {
            path: expected.path.clone(),
            reason: format!("invalid artifact identity: {error}"),
        };
    }
    let identity_matches = parse_uuid("expected brain_uuid", &expected.brain_uuid)
        .and_then(|expected_brain| {
            parse_uuid("observed brain_uuid", &descriptor.brain_uuid)
                .map(|observed_brain| expected_brain == observed_brain)
        })
        .and_then(|brain_matches| {
            parse_uuid("expected publication_uuid", &expected.publication_uuid).and_then(
                |expected_publication| {
                    parse_uuid("observed publication_uuid", &descriptor.publication_uuid).map(
                        |observed_publication| {
                            brain_matches && expected_publication == observed_publication
                        },
                    )
                },
            )
        });
    match identity_matches {
        Ok(false) => {
            return ArtifactState::ForeignIdentity {
                path: expected.path.clone(),
                expected_brain_uuid: expected.brain_uuid.clone(),
                observed_brain_uuid: descriptor.brain_uuid.clone(),
                expected_publication_uuid: expected.publication_uuid.clone(),
                observed_publication_uuid: descriptor.publication_uuid.clone(),
            };
        }
        Err(error) => {
            return ArtifactState::Corrupt {
                path: expected.path.clone(),
                reason: error.to_string(),
            };
        }
        Ok(true) => {}
    }
    if descriptor.path != expected.path
        || descriptor.kind != expected.kind
        || descriptor.artifact_schema_version != expected.artifact_schema_version
        || descriptor.algorithm_fingerprint != expected.algorithm_fingerprint
    {
        return ArtifactState::Incompatible {
            path: expected.path.clone(),
            reason: format!(
                "expected kind/schema/fingerprint {:?}/{}/{}, observed {:?}/{}/{} at {}",
                expected.kind,
                expected.artifact_schema_version,
                expected.algorithm_fingerprint,
                descriptor.kind,
                descriptor.artifact_schema_version,
                descriptor.algorithm_fingerprint,
                descriptor.path
            ),
        };
    }
    if descriptor.source_graph_generation != expected.source_graph_generation {
        return ArtifactState::Stale {
            path: expected.path.clone(),
            expected_generation: expected.source_graph_generation,
            observed_generation: descriptor.source_graph_generation,
        };
    }
    ArtifactState::Ready
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CurrentPublicationPointer {
    pub version: u32,
    pub brain_uuid: String,
    pub publication_uuid: String,
    #[serde(default)]
    pub expected_previous_publication_uuid: Option<String>,
    /// Present only when this pointer was produced by rollback. It records the
    /// abandoned publication and makes rollback deliberately one-way: another
    /// rollback is refused until a fresh activation writes a normal pointer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rolled_back_from_publication_uuid: Option<String>,
    pub manifest_blake3: String,
    pub checksum_blake3: String,
}

#[derive(Serialize)]
struct PointerPayload<'a> {
    version: u32,
    brain_uuid: &'a str,
    publication_uuid: &'a str,
    expected_previous_publication_uuid: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    rolled_back_from_publication_uuid: Option<&'a str>,
    manifest_blake3: &'a str,
}

impl CurrentPublicationPointer {
    pub fn new(
        identity: &nestweaver_store::PublicationIdentity,
        expected_previous_publication_uuid: Option<String>,
        manifest_blake3: String,
    ) -> anyhow::Result<Self> {
        identity
            .validate()
            .map_err(|error| anyhow::anyhow!("invalid publication identity: {error}"))?;
        validate_digest("manifest_blake3", &manifest_blake3)?;
        if let Some(previous) = expected_previous_publication_uuid.as_deref() {
            let previous = parse_uuid("expected_previous_publication_uuid", previous)?;
            let next = parse_uuid("publication_uuid", &identity.publication_uuid)?;
            if previous == next {
                anyhow::bail!(
                    "expected previous publication UUID must differ from the new publication UUID"
                );
            }
        }
        let mut pointer = Self {
            version: CURRENT_POINTER_VERSION,
            brain_uuid: identity.brain_uuid.clone(),
            publication_uuid: identity.publication_uuid.clone(),
            expected_previous_publication_uuid,
            rolled_back_from_publication_uuid: None,
            manifest_blake3,
            checksum_blake3: String::new(),
        };
        pointer.checksum_blake3 = pointer.payload_digest()?;
        Ok(pointer)
    }

    fn after_rollback(
        identity: &nestweaver_store::PublicationIdentity,
        rolled_back_from_publication_uuid: String,
        manifest_blake3: String,
    ) -> anyhow::Result<Self> {
        let mut pointer = Self::new(identity, None, manifest_blake3)?;
        pointer.rolled_back_from_publication_uuid = Some(rolled_back_from_publication_uuid);
        pointer.checksum_blake3 = pointer.payload_digest()?;
        pointer.validate()?;
        Ok(pointer)
    }

    fn payload_digest(&self) -> anyhow::Result<String> {
        let bytes = serde_json::to_vec(&PointerPayload {
            version: self.version,
            brain_uuid: &self.brain_uuid,
            publication_uuid: &self.publication_uuid,
            expected_previous_publication_uuid: self.expected_previous_publication_uuid.as_deref(),
            rolled_back_from_publication_uuid: self.rolled_back_from_publication_uuid.as_deref(),
            manifest_blake3: &self.manifest_blake3,
        })?;
        Ok(crate::hash::blake3_hex_bytes(&bytes))
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        if self.version != CURRENT_POINTER_VERSION {
            anyhow::bail!(
                "unsupported CURRENT pointer version {} (supported: {CURRENT_POINTER_VERSION})",
                self.version
            );
        }
        let identity = nestweaver_store::PublicationIdentity {
            brain_uuid: self.brain_uuid.clone(),
            publication_uuid: self.publication_uuid.clone(),
        };
        identity
            .validate()
            .map_err(|error| anyhow::anyhow!("invalid CURRENT identity: {error}"))?;
        validate_digest("manifest_blake3", &self.manifest_blake3)?;
        validate_digest("checksum_blake3", &self.checksum_blake3)?;
        if let Some(previous) = self.expected_previous_publication_uuid.as_deref() {
            let previous = parse_uuid("expected_previous_publication_uuid", previous)?;
            let current = parse_uuid("publication_uuid", &self.publication_uuid)?;
            if previous == current {
                anyhow::bail!(
                    "CURRENT expected previous publication UUID equals its selected publication"
                );
            }
        }
        if let Some(abandoned) = self.rolled_back_from_publication_uuid.as_deref() {
            let abandoned = parse_uuid("rolled_back_from_publication_uuid", abandoned)?;
            let current = parse_uuid("publication_uuid", &self.publication_uuid)?;
            if abandoned == current {
                anyhow::bail!("rollback source UUID equals the selected publication UUID");
            }
            if self.expected_previous_publication_uuid.is_some() {
                anyhow::bail!(
                    "a rolled-back CURRENT pointer cannot advertise another rollback predecessor"
                );
            }
        }
        let actual = self.payload_digest()?;
        if actual != self.checksum_blake3 {
            anyhow::bail!(
                "CURRENT checksum mismatch: expected {}, computed {actual}",
                self.checksum_blake3
            );
        }
        Ok(())
    }
}

fn parse_uuid(name: &str, value: &str) -> anyhow::Result<uuid::Uuid> {
    let parsed = uuid::Uuid::parse_str(value)
        .map_err(|error| anyhow::anyhow!("invalid {name} '{value}': {error}"))?;
    if parsed.is_nil() {
        anyhow::bail!("invalid {name}: nil UUID is not a data identity");
    }
    Ok(parsed)
}

fn validate_digest(name: &str, value: &str) -> anyhow::Result<()> {
    if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        anyhow::bail!("invalid {name}: expected a 64-character hexadecimal BLAKE3 digest");
    }
    Ok(())
}

/// Map a database path back to the stable anchor that names its brain.
///
/// This is the syntactic inverse of [`resolve_selected_database`]: given a
/// selected slot graph `<base>.publications/slots/<uuid>/graph.lbug` it
/// returns `<base>`, and it returns any other path unchanged.
///
/// Local state that identifies a *brain* — daemon instance ids, socket and
/// pidfile paths, log directories — must be derived from this anchor rather
/// than from the selected path. A publication cutover moves `CURRENT` to a
/// new slot, so a selected path is not a stable name: deriving identity from
/// it renames the daemon out from under itself on every cutover, orphaning
/// the running process and making `daemon status`/`daemon stop` report a
/// different instance than the one actually serving the brain (nw-145).
///
/// This is deliberately syntactic: no filesystem access, no CURRENT read.
/// It must keep working for a slot whose manifest is unreadable, which is
/// precisely when an operator needs to stop the daemon.
pub fn instance_anchor_database(db_path: &Path) -> PathBuf {
    let is_graph = db_path
        .file_name()
        .is_some_and(|n| n == PUBLICATION_GRAPH_FILE);
    if !is_graph {
        return db_path.to_path_buf();
    }
    // <root>/slots/<uuid>/graph.lbug — require the `slots` component so an
    // unrelated file that happens to be named graph.lbug is left alone.
    let Some(slot_dir) = db_path.parent() else {
        return db_path.to_path_buf();
    };
    let Some(slots_dir) = slot_dir.parent() else {
        return db_path.to_path_buf();
    };
    if slots_dir.file_name() != Some(std::ffi::OsStr::new("slots")) {
        return db_path.to_path_buf();
    }
    let Some(root) = slots_dir.parent() else {
        return db_path.to_path_buf();
    };
    // The root is `sidecar_path(base, ".publications")`, i.e. the suffix is
    // appended to the whole base path, so strip it from the OS string rather
    // than treating it as a file extension.
    let root_os = root.as_os_str().as_encoded_bytes();
    match root_os.strip_suffix(b".publications") {
        Some(base) if !base.is_empty() => {
            // SAFETY: `base` is a prefix of bytes returned by
            // `as_encoded_bytes` split at an ASCII boundary, which the
            // documented safety contract of `from_encoded_bytes_unchecked`
            // permits.
            let base = unsafe { std::ffi::OsStr::from_encoded_bytes_unchecked(base) };
            PathBuf::from(base)
        }
        _ => db_path.to_path_buf(),
    }
}

pub fn current_pointer_path(publication_root: &Path) -> PathBuf {
    publication_root.join("CURRENT")
}

pub fn slot_path(publication_root: &Path, publication_uuid: &str) -> anyhow::Result<PathBuf> {
    let publication_uuid = parse_uuid("publication_uuid", publication_uuid)?;
    Ok(publication_root
        .join("slots")
        .join(publication_uuid.to_string()))
}

pub fn operation_path(publication_root: &Path, operation_uuid: &str) -> anyhow::Result<PathBuf> {
    let operation_uuid = parse_uuid("operation_uuid", operation_uuid)?;
    Ok(publication_root
        .join("operations")
        .join(operation_uuid.to_string()))
}

pub fn default_publication_root(db_path: &Path) -> PathBuf {
    crate::sidecar_path(db_path, ".publications")
}

/// POSIX record locks do not conflict with another descriptor in the same
/// process. Claim the canonical root before opening its lock file so a second
/// in-process publication operation cannot slip past that compatibility gate.
/// Spawned children are expected to exec before entering NestWeaver code; a
/// fork-only child inherits a stale copy of this process-local registry.
static PROCESS_PUBLICATION_ROOTS: std::sync::OnceLock<
    std::sync::Mutex<std::collections::HashSet<PathBuf>>,
> = std::sync::OnceLock::new();

#[derive(Debug)]
struct ProcessPublicationRootClaim {
    path: PathBuf,
}

impl ProcessPublicationRootClaim {
    fn acquire(path: &Path) -> Option<Self> {
        let mut claimed = PROCESS_PUBLICATION_ROOTS
            .get_or_init(|| std::sync::Mutex::new(std::collections::HashSet::new()))
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        claimed.insert(path.to_path_buf()).then(|| Self {
            path: path.to_path_buf(),
        })
    }
}

impl Drop for ProcessPublicationRootClaim {
    fn drop(&mut self) {
        PROCESS_PUBLICATION_ROOTS
            .get_or_init(|| std::sync::Mutex::new(std::collections::HashSet::new()))
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&self.path);
    }
}

/// Cross-process exclusion for one publication root.
///
/// The `IndexPublicationLease` is an in-process coordinator — a `Mutex` plus a
/// `Condvar` owned by a single `GraphStore`. Two processes, or even two stores
/// opened in the same process, receive UNRELATED leases, so it cannot serialize
/// anything across process boundaries. Nor can a database write lock stand in
/// for it here: activation locks the SELECTED slot graph while a prune inspects
/// the base, so the two lock different files and exclude nothing.
///
/// Anchoring the lock to the publication ROOT is what makes it correct, because
/// the root is the one thing every publication operation shares — and it is
/// also what makes `--root` safe, since the lock follows whichever root the
/// caller actually operates on.
///
/// Held by rebuild, rollback, discard and prune. Released on drop.
#[derive(Debug)]
pub struct PublicationRootLock {
    _file: std::fs::File,
    root: PathBuf,
    path: PathBuf,
    // Declared last so the OS lock closes before another thread in this
    // process can claim the root.
    _process_claim: ProcessPublicationRootClaim,
}

impl PublicationRootLock {
    /// Take the lock, or fail fast if another operation holds it.
    ///
    /// A live external owner is refused without queueing: a publication rebuild
    /// can run for minutes, and a CLI must report that contention rather than
    /// silently wait. On Unix, acquisition may retry for at most 500 ms so a
    /// stale flock description in another thread's fork-before-exec window
    /// cannot manufacture durable contention.
    pub fn acquire(publication_root: &Path) -> anyhow::Result<Self> {
        std::fs::create_dir_all(publication_root).with_context(|| {
            format!(
                "create publication root {} for locking",
                publication_root.display()
            )
        })?;
        let canonical_root = std::fs::canonicalize(publication_root).with_context(|| {
            format!(
                "resolve publication root {} for locking",
                publication_root.display()
            )
        })?;
        let path = canonical_root.join("LOCK");
        let process_claim = ProcessPublicationRootClaim::acquire(&path).ok_or_else(|| {
            publication_lock_contention(&path, std::io::ErrorKind::WouldBlock.into())
        })?;
        let file = std::fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&path)
            .with_context(|| format!("open publication lock {}", path.display()))?;
        lock_publication_file(&file).map_err(|error| publication_lock_contention(&path, error))?;
        Ok(Self {
            _file: file,
            root: canonical_root,
            path,
            _process_claim: process_claim,
        })
    }

    /// The lock file backing this guard, for diagnostics.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Whether this guard covers exactly this publication root after resolving
    /// relative paths and symlink aliases.
    pub fn authorizes(&self, publication_root: &Path) -> bool {
        std::fs::canonicalize(publication_root).is_ok_and(|root| root == self.root)
    }

    fn ensure_authorizes(&self, publication_root: &Path) -> anyhow::Result<&Path> {
        if self.authorizes(publication_root) {
            Ok(&self.root)
        } else {
            anyhow::bail!(
                "publication root lock for {} does not authorize {}",
                self.root.display(),
                publication_root.display()
            )
        }
    }
}

fn publication_lock_contention(path: &Path, error: std::io::Error) -> anyhow::Error {
    anyhow::anyhow!(
        "another publication operation holds {} ({error}); wait for it to finish, \
         or check `nestweaver publication status`",
        path.display()
    )
}

#[cfg(unix)]
fn lock_publication_file(file: &std::fs::File) -> std::io::Result<()> {
    // Keep this inode on one lock interface. Linux treats flock and POSIX
    // record locks independently, while macOS makes them cooperate; layering
    // both on one descriptor can therefore contend with this process's own
    // lock. Publication roots have always coordinated through flock, so that
    // is also the protocol compatible with a live pre-upgrade process. The
    // process-local claim closes the same-process duplicate-descriptor gap.
    //
    // A flock is inherited across fork. Rust descriptors are CLOEXEC, but
    // another thread's child can briefly keep a just-dropped owner's open file
    // description alive until exec, so bound the retry instead of turning that
    // window into false durable contention.
    for attempt in 0..=100 {
        match file.try_lock() {
            Ok(()) => return Ok(()),
            Err(std::fs::TryLockError::WouldBlock) if attempt < 100 => {
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            Err(std::fs::TryLockError::WouldBlock) => {
                return Err(std::io::ErrorKind::WouldBlock.into());
            }
            Err(std::fs::TryLockError::Error(error)) => return Err(error),
        }
    }
    unreachable!("bounded publication flock retry always returns")
}

#[cfg(not(unix))]
fn lock_publication_file(file: &std::fs::File) -> std::io::Result<()> {
    file.try_lock().map_err(Into::into)
}

/// One slot considered by [`prune_slots`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlotDisposition {
    pub publication_uuid: String,
    pub bytes: u64,
    /// `None` when the slot is reclaimable; `Some(reason)` when it is retained.
    pub retained_because: Option<String>,
}

/// What a [`prune_slots`] pass found and (unless `dry_run`) removed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SlotPruneReport {
    pub slots: Vec<SlotDisposition>,
    pub removed_bytes: u64,
    pub dry_run: bool,
}

impl SlotPruneReport {
    pub fn removed(&self) -> impl Iterator<Item = &SlotDisposition> {
        self.slots
            .iter()
            .filter(|slot| slot.retained_because.is_none())
    }
    pub fn retained(&self) -> impl Iterator<Item = &SlotDisposition> {
        self.slots
            .iter()
            .filter(|slot| slot.retained_because.is_some())
    }
}

fn directory_bytes(path: &Path) -> u64 {
    let mut total = 0;
    let Ok(entries) = std::fs::read_dir(path) else {
        return 0;
    };
    for entry in entries.flatten() {
        match entry.file_type() {
            Ok(kind) if kind.is_dir() => total += directory_bytes(&entry.path()),
            Ok(kind) if kind.is_file() => {
                total += entry.metadata().map(|meta| meta.len()).unwrap_or(0)
            }
            _ => {}
        }
    }
    total
}

/// Reclaim publication slots that nothing can still reach.
///
/// nw-135: the `slots` directory was never ENUMERATED anywhere in the repo —
/// no GC, no retention sweep, no orphan detection. `discard_operation` refuses
/// anything not cancelled-or-failed, so a slot from a SUCCESSFUL rebuild could
/// never be removed in-tool at all. Measured on a scratch root, four rebuilds
/// left all four full slots on disk forever; on the real brain a slot is
/// ~1.2 GB, which is roughly 55 rebuilds to exhaustion.
///
/// Three things are retained, and nothing else is:
///
/// 1. The slot CURRENT selects — deleting it would destroy the live graph.
/// 2. Its retained predecessor, which the documented one-step rollback
///    contract needs. Exactly one predecessor is required; every slot beyond
///    that is pure leak.
/// 3. Any slot targeted by an operation journal that still exists, including
///    unreadable ones. A journal we cannot parse cannot tell us which slot it
///    targeted, so every slot stays until that journal is discarded — the same
///    conservative choice `discard_invalid_operation` already makes.
pub fn prune_slots(
    publication_root: &Path,
    lock: &PublicationRootLock,
    dry_run: bool,
) -> anyhow::Result<SlotPruneReport> {
    // Proof of CROSS-PROCESS exclusivity, in the signature so a caller cannot
    // forget it. An earlier version took an `IndexPublicationLease` instead,
    // which looked like exclusion but is not: that lease is an in-process
    // Mutex/Condvar owned by one GraphStore, so a rebuild in another process
    // holds an unrelated one. This lock is anchored to the same root being
    // pruned, which is what actually serializes against rebuild, rollback and
    // discard — and what stops a cutover landing between the CURRENT read below
    // and the deletes further down, which would reclaim the slot that cutover
    // had just selected.
    // Continue through the canonical root held by the guard. Besides making
    // authorization exact in release builds, this keeps a symlink alias from
    // being retargeted between the coverage check and the filesystem work.
    let publication_root = lock.ensure_authorizes(publication_root)?;
    let slots_dir = publication_root.join("slots");
    let entries = match std::fs::read_dir(&slots_dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(SlotPruneReport {
                dry_run,
                ..Default::default()
            });
        }
        Err(error) => {
            return Err(error).with_context(|| format!("read slots {}", slots_dir.display()));
        }
    };

    let mut retained: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    if let Some(pointer) = read_current(publication_root)? {
        retained.insert(
            pointer.publication_uuid.clone(),
            "selected by CURRENT".to_string(),
        );
        if let Some(previous) = pointer.expected_previous_publication_uuid.as_ref() {
            retained
                .entry(previous.clone())
                .or_insert_with(|| "retained predecessor for one-step rollback".to_string());
        }
    }
    let operations = crate::publication_operation::list_operations(publication_root)?;
    for operation in &operations.operations {
        // Only IN-FLIGHT work pins a slot. A terminal journal (Activated or
        // Cancelled) describes work that is over, and its slot's liveness is
        // then decided solely by CURRENT and the retained predecessor above.
        //
        // Pinning terminal journals too was the original bug here: journals
        // survive activation — only discard_operation removes one, and it
        // refuses anything not cancelled-or-failed — so EVERY published slot
        // kept a journal naming it, every slot was retained, and the prune
        // reclaimed nothing at all. That is exactly the superseded-slot leak
        // nw-135 exists to close, so the retention rule cancelled out the
        // feature.
        if operation.phase.is_terminal() {
            continue;
        }
        retained
            .entry(operation.plan.target_publication_uuid.clone())
            .or_insert_with(|| {
                format!(
                    "targeted by in-flight operation {} ({:?})",
                    operation.plan.operation_uuid, operation.phase
                )
            });
    }
    // An unreadable journal cannot name its target, so nothing may be reclaimed
    // while one exists.
    let blocked_by_invalid = operations
        .invalid_operations
        .first()
        .map(|invalid| invalid.operation_uuid.clone());

    let mut report = SlotPruneReport {
        dry_run,
        ..Default::default()
    };
    for entry in entries {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let publication_uuid = entry.file_name().to_string_lossy().into_owned();
        let path = entry.path();
        let bytes = directory_bytes(&path);
        let retained_because = retained.get(&publication_uuid).cloned().or_else(|| {
            blocked_by_invalid
                .as_ref()
                .map(|uuid| format!("unreadable operation journal {uuid} may still target it"))
        });
        if retained_because.is_none() && !dry_run {
            std::fs::remove_dir_all(&path)
                .with_context(|| format!("remove slot {}", path.display()))?;
            nestweaver_store::durable_sidecar::sync_parent_directory_durable(&path)?;
        }
        if retained_because.is_none() {
            report.removed_bytes += bytes;
        }
        report.slots.push(SlotDisposition {
            publication_uuid,
            bytes,
            retained_because,
        });
    }
    report
        .slots
        .sort_by(|a, b| a.publication_uuid.cmp(&b.publication_uuid));
    Ok(report)
}

/// denotes the implicit base database used before the first cutover.
pub fn retained_predecessor_database(
    base_db_path: &Path,
    predecessor_publication_uuid: Option<&str>,
) -> anyhow::Result<PathBuf> {
    let Some(predecessor) = predecessor_publication_uuid else {
        return Ok(base_db_path.to_path_buf());
    };
    Ok(
        slot_path(&default_publication_root(base_db_path), predecessor)?
            .join(PUBLICATION_GRAPH_FILE),
    )
}

pub fn read_current(publication_root: &Path) -> anyhow::Result<Option<CurrentPublicationPointer>> {
    let path = current_pointer_path(publication_root);
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).with_context(|| format!("read CURRENT pointer {}", path.display()));
        }
    };
    let pointer: CurrentPublicationPointer = serde_json::from_slice(&bytes)
        .with_context(|| format!("parse CURRENT pointer {}", path.display()))?;
    pointer
        .validate()
        .with_context(|| format!("validate CURRENT pointer {}", path.display()))?;
    Ok(Some(pointer))
}

/// Resolve the database selected by a publication root without trusting the
/// pointer alone. An absent `CURRENT` preserves the legacy/base database; a
/// selected slot is admitted only after its manifest, identity, graph
/// descriptor, and graph-owned identity agree. Full payload checksums are performed by
/// the validation/activation path; repeating a multi-gigabyte graph hash on
/// every short-lived CLI invocation would make the selector itself O(graph).
pub fn resolve_selected_database(base_db_path: &Path) -> anyhow::Result<PathBuf> {
    let publication_root = default_publication_root(base_db_path);
    let Some(pointer) = read_current(&publication_root)? else {
        return Ok(base_db_path.to_path_buf());
    };
    let slot = slot_path(&publication_root, &pointer.publication_uuid)?;
    let manifest_path = slot.join(PUBLICATION_MANIFEST_FILE);
    let manifest_bytes = std::fs::read(&manifest_path).with_context(|| {
        format!(
            "read selected publication manifest {}",
            manifest_path.display()
        )
    })?;
    if crate::hash::blake3_hex_bytes(&manifest_bytes) != pointer.manifest_blake3 {
        anyhow::bail!("selected publication manifest no longer matches CURRENT");
    }
    let bundle: PublicationBundleV3 =
        serde_json::from_slice(&manifest_bytes).with_context(|| {
            format!(
                "parse selected publication manifest {}",
                manifest_path.display()
            )
        })?;
    bundle.validate_metadata(crate::snapshot::SNAPSHOT_FORMAT_VERSION)?;
    if parse_uuid("CURRENT brain_uuid", &pointer.brain_uuid)?
        != parse_uuid("bundle brain_uuid", &bundle.brain_uuid)?
        || parse_uuid("CURRENT publication_uuid", &pointer.publication_uuid)?
            != parse_uuid("bundle publication_uuid", &bundle.publication_uuid)?
    {
        anyhow::bail!("selected publication manifest identity does not match CURRENT");
    }
    let mut graph_artifacts = bundle
        .artifacts
        .iter()
        .filter(|artifact| artifact.kind == ArtifactKind::Graph);
    let graph = graph_artifacts
        .next()
        .ok_or_else(|| anyhow::anyhow!("selected publication has no graph artifact"))?;
    if graph_artifacts.next().is_some() {
        anyhow::bail!("selected publication has more than one graph artifact");
    }
    let graph_path = slot.join(&graph.path);
    let metadata = std::fs::metadata(&graph_path).with_context(|| {
        format!(
            "inspect selected publication graph {}",
            graph_path.display()
        )
    })?;
    if !metadata.is_file() {
        anyhow::bail!("selected publication graph is not a regular file");
    }
    // A selected local graph remains writable after cutover, so its live size
    // and checksum legitimately advance beyond the sealed baseline. The graph
    // identity is the stable binding that must never change.
    let store = nestweaver_store::GraphStore::open_read_only_without_migration(&graph_path)
        .map_err(|error| anyhow::anyhow!("open selected publication graph: {error}"))?;
    let identity = store
        .publication_identity()
        .map_err(|error| anyhow::anyhow!("read selected publication identity: {error}"))?
        .ok_or_else(|| anyhow::anyhow!("selected publication graph has no identity"))?;
    if parse_uuid("selected graph brain_uuid", &identity.brain_uuid)?
        != parse_uuid("CURRENT brain_uuid", &pointer.brain_uuid)?
        || parse_uuid(
            "selected graph publication_uuid",
            &identity.publication_uuid,
        )? != parse_uuid("CURRENT publication_uuid", &pointer.publication_uuid)?
    {
        anyhow::bail!("selected publication graph identity does not match CURRENT");
    }
    Ok(graph_path)
}

/// Durably select `next` when the currently selected publication UUID equals
/// `expected_current`. The caller must hold the incumbent graph's publication
/// lease, which serializes switch attempts with graph/sidecar publication.
///
/// The target slot's canonical `publication.json` must already exist and hash
/// to `next.manifest_blake3`; a pointer can never select a missing or differently
/// sealed slot.
pub fn compare_and_swap_current(
    publication_root: &Path,
    lease: &nestweaver_store::IndexPublicationLease<'_>,
    expected_current: Option<&str>,
    next: &CurrentPublicationPointer,
) -> anyhow::Result<()> {
    lease
        .ensure_clean_for_snapshot()
        .map_err(|error| anyhow::anyhow!("refusing CURRENT switch from dirty graph: {error}"))?;
    next.validate()?;

    let incumbent = lease
        .store()
        .publication_identity()
        .map_err(|error| anyhow::anyhow!("read incumbent publication identity: {error}"))?
        .ok_or_else(|| anyhow::anyhow!("incumbent graph has no publication identity"))?;
    if parse_uuid("incumbent brain_uuid", &incumbent.brain_uuid)?
        != parse_uuid("next brain_uuid", &next.brain_uuid)?
    {
        anyhow::bail!(
            "refusing CURRENT switch across brains: incumbent is {}, target is {}",
            incumbent.brain_uuid,
            next.brain_uuid
        );
    }

    let current = read_current(publication_root)?;
    let observed = current
        .as_ref()
        .map(|pointer| parse_uuid("current publication_uuid", &pointer.publication_uuid))
        .transpose()?;
    let expected = expected_current
        .map(|value| parse_uuid("expected current publication_uuid", value))
        .transpose()?;
    if observed != expected {
        // Permanent: the expected predecessor will never be CURRENT again, so
        // retrying this operation can only re-observe the same conflict
        // (nw-148).
        return Err(
            crate::publication_operation::PermanentPublicationFailure(format!(
                "CURRENT compare-and-swap conflict: expected {}, observed {}",
                expected
                    .map(|value| value.to_string())
                    .unwrap_or_else(|| "<none>".to_string()),
                observed
                    .map(|value| value.to_string())
                    .unwrap_or_else(|| "<none>".to_string())
            ))
            .into(),
        );
    }
    let declared_previous = next
        .expected_previous_publication_uuid
        .as_deref()
        .map(|value| parse_uuid("declared previous publication_uuid", value))
        .transpose()?;
    let rollback_source = next
        .rolled_back_from_publication_uuid
        .as_deref()
        .map(|value| parse_uuid("rolled_back_from_publication_uuid", value))
        .transpose()?;
    let predecessor_contract_holds = if rollback_source.is_some() {
        rollback_source == expected && declared_previous.is_none()
    } else {
        declared_previous == expected
    };
    if !predecessor_contract_holds {
        anyhow::bail!(
            "CURRENT pointer declares previous {} and rollback source {}, but compare-and-swap expects {}",
            declared_previous
                .map(|value| value.to_string())
                .unwrap_or_else(|| "<none>".to_string()),
            rollback_source
                .map(|value| value.to_string())
                .unwrap_or_else(|| "<none>".to_string()),
            expected
                .map(|value| value.to_string())
                .unwrap_or_else(|| "<none>".to_string())
        );
    }

    let slot = slot_path(publication_root, &next.publication_uuid)?;
    let manifest = slot.join(PUBLICATION_MANIFEST_FILE);
    let manifest_bytes = std::fs::read(&manifest)
        .with_context(|| format!("read target publication manifest {}", manifest.display()))?;
    let manifest_digest = crate::hash::blake3_hex_bytes(&manifest_bytes);
    if manifest_digest != next.manifest_blake3 {
        anyhow::bail!(
            "target publication manifest digest mismatch: CURRENT declares {}, slot contains {manifest_digest}",
            next.manifest_blake3
        );
    }

    std::fs::create_dir_all(publication_root)?;
    let path = current_pointer_path(publication_root);
    let bytes = serde_json::to_vec_pretty(next)?;
    nestweaver_store::durable_sidecar::atomic_replace_file(&path, |file| {
        file.write_all(&bytes)?;
        file.write_all(b"\n")
    })
    .with_context(|| format!("durably replace CURRENT pointer {}", path.display()))
}

/// Whether a slot's artifact bytes are still bound to the digests its
/// manifest declares.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ArtifactBytes {
    /// Activation: the slot was just sealed and nothing has been allowed to
    /// open it for writing, so every artifact must still hash to its
    /// descriptor.
    Verified,
    /// Rollback: the target was previously selected, and a selected graph
    /// stays writable after cutover (see `resolve_selected_database`), so its
    /// live size and checksum legitimately advance past the sealed baseline.
    /// Shape is still checkable; bytes are not.
    Live,
}

/// Enforce the structural guarantees a slot must hold before a pointer may
/// select it, for every route into publication (nw-253).
///
/// nw-149 gave the activation route three guards -- a symlink refusal, a
/// per-artifact size/digest check, and a reverse inventory proving the slot
/// holds nothing the manifest does not describe -- but wired them only into
/// `validate_target_slot`, whose two call sites are both activations
/// (`mark_ready`, `activate_operation`). Rollback reached
/// `compare_and_swap_current` having checked the manifest's own metadata and
/// identity and nothing about the directory those bytes describe, so a
/// predecessor slot whose artifacts had been replaced with symbolic links was
/// admitted. The downstream reader does not compensate:
/// `resolve_selected_database` inspects the graph artifact with
/// `std::fs::metadata`, which FOLLOWS links, so `is_file()` is true of a link
/// pointing anywhere on the filesystem.
///
/// `ArtifactBytes` is the one honest asymmetry between the routes. Everything
/// else -- containment, "real files only", `described == present` -- is a
/// property of how a slot is WRITTEN, and the publisher never writes a link or
/// an undescribed file on either route.
pub(crate) fn validate_slot_contents(
    slot: &Path,
    bundle: &PublicationBundleV3,
    bytes: ArtifactBytes,
) -> anyhow::Result<()> {
    use crate::publication_operation::PermanentPublicationFailure;

    for descriptor in &bundle.artifacts {
        let artifact = slot.join(&descriptor.path);
        // Hashing OPENS the path, and opening follows symlinks — so a described
        // artifact that is a symlink would validate against bytes living
        // outside the slot entirely, while the reverse inventory below (which
        // does not follow links) could not see it. A sealed slot contains
        // exactly what it declares; a link is not a shape we ever write.
        let metadata = std::fs::symlink_metadata(&artifact).map_err(|error| {
            anyhow::anyhow!("stat target artifact {}: {error}", artifact.display())
        })?;
        if metadata.file_type().is_symlink() {
            return Err(PermanentPublicationFailure(format!(
                "target artifact {} is a symbolic link; a sealed slot must contain \
                 real files only",
                descriptor.path
            ))
            .into());
        }
        // Belt and braces on containment: `validate_metadata` rejects absolute
        // and `..` paths, but the bytes about to be trusted are worth proving
        // are inside the slot rather than inferring it.
        if !artifact.starts_with(slot) {
            return Err(PermanentPublicationFailure(format!(
                "target artifact {} resolves outside the publication slot",
                descriptor.path
            ))
            .into());
        }
        if bytes == ArtifactBytes::Verified {
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
    }

    // nw-149: and the REVERSE. The loop above only proves every DESCRIBED file
    // is present and intact; it says nothing about files present but not
    // described, so anything dropped into a sealed slot was invisible to
    // validation and travelled with the publication. `validate_backup_publication_inventory`
    // already required `described == present` for the same artifacts — this
    // path simply never got the other half.
    //
    // Filed as a hardening asymmetry rather than a proven escape: the probe
    // matrix achieved no exploit. It closes a gap in an invariant; it does not
    // patch a known attack.
    let described: std::collections::BTreeSet<&str> = bundle
        .artifacts
        .iter()
        .map(|descriptor| descriptor.path.as_str())
        .collect();
    // Walk RECURSIVELY and compare the same shape the manifest uses. Artifact
    // paths are relative and `/`-joined (`build_backup_publication_bundle` ->
    // `normalized_relative_path`), and nested ones are routine: the BM25 and
    // regex sidecars are whole DIRECTORIES, described file by file as
    // `<db>.tantivy/meta.json` and friends. A non-recursive `read_dir` would
    // compare the bare directory name `<db>.tantivy` against those paths,
    // match nothing, and fail every real publication permanently.
    let mut present: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for entry in walkdir::WalkDir::new(slot).into_iter() {
        let entry = entry
            .map_err(|error| anyhow::anyhow!("read target slot {}: {error}", slot.display()))?;
        // `WalkDir` does not follow symlinks, so a link reports neither file
        // nor dir — it would fall through this filter and be INVISIBLE to the
        // undescribed-file check while still being openable by the digest
        // check above. Refuse it explicitly instead.
        if entry.file_type().is_symlink() {
            return Err(PermanentPublicationFailure(format!(
                "target slot contains a symbolic link ({}); a sealed slot must contain \
                 real files only",
                entry.path().display()
            ))
            .into());
        }
        // Directories are containers, not artifacts; the manifest describes the
        // files inside them.
        if !entry.file_type().is_file() {
            continue;
        }
        let relative = entry
            .path()
            .strip_prefix(slot)
            .map_err(|error| anyhow::anyhow!("target slot entry outside the slot: {error}"))?;
        let mut components = Vec::new();
        for component in relative.components() {
            match component {
                std::path::Component::Normal(value) => {
                    components.push(value.to_string_lossy().into_owned())
                }
                // A slot is written by us; anything else is not a shape we
                // describe, so refuse rather than guess at a normal form.
                _ => anyhow::bail!("unsafe target slot entry path: {}", entry.path().display()),
            }
        }
        let name = components.join("/");
        // The manifest describes the artifacts; it does not describe itself.
        if name == PUBLICATION_MANIFEST_FILE {
            continue;
        }
        present.insert(name);
    }
    let undescribed: Vec<&String> = present
        .iter()
        .filter(|name| !described.contains(name.as_str()))
        .collect();
    if !undescribed.is_empty() {
        // Permanent for the same reason as a digest mismatch: a retry sees the
        // same directory.
        return Err(PermanentPublicationFailure(format!(
            "target slot contains {} file(s) the manifest does not describe: {:?}; \
             a sealed slot must contain exactly what it declares",
            undescribed.len(),
            undescribed
        ))
        .into());
    }

    Ok(())
}

/// Roll back exactly one selected publication while the active graph is
/// quiesced. The first cutover returns to the implicit legacy/base database by
/// removing `CURRENT`; later cutovers select the retained predecessor slot.
/// A stale caller can never roll back a newer selection.
pub fn rollback_current(
    publication_root: &Path,
    lease: &nestweaver_store::IndexPublicationLease<'_>,
    expected_current: &str,
) -> anyhow::Result<Option<CurrentPublicationPointer>> {
    let root_lock = PublicationRootLock::acquire(publication_root)?;
    rollback_current_under_lock(publication_root, lease, expected_current, &root_lock)
}

/// Roll back while the caller holds publication-root ownership across an
/// external quiescence proof and this selector mutation. Daemon startup takes
/// the same lock while resolving/opening CURRENT, closing the check-to-CAS
/// race without coupling the engine to process-lifecycle implementation.
pub fn rollback_current_under_lock(
    publication_root: &Path,
    lease: &nestweaver_store::IndexPublicationLease<'_>,
    expected_current: &str,
    root_lock: &PublicationRootLock,
) -> anyhow::Result<Option<CurrentPublicationPointer>> {
    let publication_root = root_lock.ensure_authorizes(publication_root)?;
    lease
        .ensure_clean_for_snapshot()
        .map_err(|error| anyhow::anyhow!("refusing CURRENT rollback from dirty graph: {error}"))?;
    let current = read_current(publication_root)?
        .ok_or_else(|| anyhow::anyhow!("no selected publication to roll back"))?;
    if let Some(abandoned) = current.rolled_back_from_publication_uuid.as_deref() {
        anyhow::bail!(
            "CURRENT already represents the one-step rollback from {abandoned}; activate a fresh publication before another rollback"
        );
    }
    if parse_uuid("expected current publication_uuid", expected_current)?
        != parse_uuid(
            "observed current publication_uuid",
            &current.publication_uuid,
        )?
    {
        anyhow::bail!(
            "CURRENT rollback conflict: expected {}, observed {}",
            expected_current,
            current.publication_uuid
        );
    }
    let store_identity = lease
        .store()
        .publication_identity()
        .map_err(|error| anyhow::anyhow!("read rollback graph identity: {error}"))?
        .ok_or_else(|| anyhow::anyhow!("rollback graph has no publication identity"))?;
    if parse_uuid("rollback brain_uuid", &store_identity.brain_uuid)?
        != parse_uuid("CURRENT brain_uuid", &current.brain_uuid)?
    {
        anyhow::bail!("refusing CURRENT rollback across brains");
    }

    let Some(previous_uuid) = current.expected_previous_publication_uuid.as_deref() else {
        let path = current_pointer_path(publication_root);
        std::fs::remove_file(&path)
            .with_context(|| format!("remove CURRENT pointer {}", path.display()))?;
        nestweaver_store::durable_sidecar::sync_parent_directory_durable(&path)?;
        return Ok(None);
    };
    let previous_slot = slot_path(publication_root, previous_uuid)?;
    let manifest_path = previous_slot.join(PUBLICATION_MANIFEST_FILE);
    let manifest_bytes = std::fs::read(&manifest_path).with_context(|| {
        format!(
            "read rollback publication manifest {}",
            manifest_path.display()
        )
    })?;
    let bundle: PublicationBundleV3 = serde_json::from_slice(&manifest_bytes)?;
    bundle.validate_metadata(crate::snapshot::SNAPSHOT_FORMAT_VERSION)?;
    if parse_uuid(
        "rollback predecessor publication_uuid",
        &bundle.publication_uuid,
    )? != parse_uuid("CURRENT predecessor publication_uuid", previous_uuid)?
        || parse_uuid("rollback predecessor brain_uuid", &bundle.brain_uuid)?
            != parse_uuid("CURRENT brain_uuid", &current.brain_uuid)?
    {
        anyhow::bail!("rollback predecessor manifest identity does not match CURRENT");
    }
    // nw-253: the predecessor slot has to satisfy the same structural
    // guarantees as an activation target. Bytes are `Live` here because this
    // slot was already selected once and its graph stayed writable after that
    // cutover.
    validate_slot_contents(&previous_slot, &bundle, ArtifactBytes::Live)?;
    let previous_identity = nestweaver_store::PublicationIdentity {
        brain_uuid: bundle.brain_uuid,
        publication_uuid: bundle.publication_uuid,
    };
    let previous = CurrentPublicationPointer::after_rollback(
        &previous_identity,
        current.publication_uuid.clone(),
        crate::hash::blake3_hex_bytes(&manifest_bytes),
    )?;
    compare_and_swap_current(
        publication_root,
        lease,
        Some(&current.publication_uuid),
        &previous,
    )?;
    Ok(Some(previous))
}

use anyhow::Context;

#[cfg(test)]
mod tests {
    use super::*;

    /// nw-263, the CROSS-CRATE half — and the one no single reviewer could see,
    /// because the two halves live in different crates.
    ///
    /// `validate_slot_contents` refuses ANY file the manifest does not describe,
    /// unconditionally (the digest check is gated on `ArtifactBytes::Verified`;
    /// the reverse inventory is not), with a `PermanentPublicationFailure` — so
    /// retries are refused too. The rollback route passes `ArtifactBytes::Live`
    /// and hits it. A slot's described set is sealed by `WalkDir` at seal time,
    /// so anything created afterwards is undescribed BY CONSTRUCTION.
    ///
    /// The Tantivy recovery lock used to be written to the index directory's
    /// parent, which for a slot-resident sidecar
    /// (`<root>/slots/<uuid>/<name>.lbug.tantivy`) is the slot root. Tantivy
    /// never removes a lock file, so one interrupted schema migration wedged
    /// rollback of that slot forever.
    ///
    /// Asserted as a PATH composition rather than by running a migration: the
    /// invariant is where the lock RESOLVES, and that is decidable without
    /// touching a filesystem. Do NOT "fix" this by exempting the lock's name
    /// from the inventory — that punches a permanent hole in a sealed-slot
    /// invariant to paper over a layout bug.
    #[test]
    fn the_tantivy_recovery_lock_never_resolves_inside_a_publication_slot() {
        let publication_root = std::path::Path::new("/var/nestweaver/publications");
        let slot = slot_path(publication_root, "11111111-2222-3333-4444-555555555555").unwrap();
        // Exactly the layout `tantivy_sidecar_path_for` produces: an OsString
        // push onto the database path, so the sidecar is a SIBLING of the
        // database file and its parent is the slot root.
        let sidecar = slot.join("brain.lbug.tantivy");

        let lock = nestweaver_store::reindex_lock_path(&sidecar);

        assert!(
            !lock.starts_with(&slot),
            "the recovery lock resolves inside a sealed publication slot, where \
             `validate_slot_contents` refuses it permanently: {}",
            lock.display()
        );
        assert!(
            !lock.starts_with(publication_root),
            "the lock must not be anywhere under the publication root either — \
             a future sweep or manifest could describe that tree too: {}",
            lock.display()
        );
    }

    fn descriptor_fixture() -> (ArtifactExpectation, ArtifactDescriptor) {
        let identity = nestweaver_store::PublicationIdentity::new_brain();
        let descriptor = ArtifactDescriptor {
            path: "bm25/meta.json".to_string(),
            kind: ArtifactKind::Bm25,
            artifact_schema_version: 3,
            byte_size: 42,
            blake3: "a".repeat(64),
            brain_uuid: identity.brain_uuid.clone(),
            publication_uuid: identity.publication_uuid.clone(),
            producer_version: "6.3.0".to_string(),
            source_graph_generation: 8,
            algorithm_fingerprint: "bm25-v3".to_string(),
        };
        let expected = ArtifactExpectation {
            path: descriptor.path.clone(),
            kind: descriptor.kind.clone(),
            artifact_schema_version: descriptor.artifact_schema_version,
            brain_uuid: descriptor.brain_uuid.clone(),
            publication_uuid: descriptor.publication_uuid.clone(),
            source_graph_generation: descriptor.source_graph_generation,
            algorithm_fingerprint: descriptor.algorithm_fingerprint.clone(),
        };
        (expected, descriptor)
    }

    #[test]
    fn artifact_states_distinguish_missing_stale_incompatible_foreign_and_corrupt() {
        let (expected, descriptor) = descriptor_fixture();
        assert_eq!(
            classify_artifact_descriptor(&expected, Ok(Some(&descriptor))),
            ArtifactState::Ready
        );
        assert!(matches!(
            classify_artifact_descriptor(&expected, Ok(None)),
            ArtifactState::Missing { .. }
        ));

        let mut stale = descriptor.clone();
        stale.source_graph_generation -= 1;
        assert!(matches!(
            classify_artifact_descriptor(&expected, Ok(Some(&stale))),
            ArtifactState::Stale { .. }
        ));

        let mut incompatible = descriptor.clone();
        incompatible.algorithm_fingerprint = "bm25-v4".to_string();
        assert!(matches!(
            classify_artifact_descriptor(&expected, Ok(Some(&incompatible))),
            ArtifactState::Incompatible { .. }
        ));

        let mut foreign = descriptor.clone();
        foreign.brain_uuid = uuid::Uuid::new_v4().to_string();
        assert!(matches!(
            classify_artifact_descriptor(&expected, Ok(Some(&foreign))),
            ArtifactState::ForeignIdentity { .. }
        ));

        assert!(matches!(
            classify_artifact_descriptor(&expected, Err("torn header".to_string())),
            ArtifactState::Corrupt { .. }
        ));
    }

    /// nw-147, second half. Recomputing the fingerprint from the parameters an
    /// artifact DECLARES proves only that the artifact agrees with itself.
    ///
    /// An artifact computed with foreign damping can declare that damping and
    /// carry the correct fingerprint FOR it, and every internal check passes —
    /// because nothing supplied an expectation from outside the artifact. The
    /// scores are then silently incomparable with the ones this build produces.
    #[test]
    fn a_self_consistent_pagerank_artifact_from_a_foreign_configuration_is_refused() {
        use nestweaver_store::artifact_envelope::{ArtifactEnvelope, ArtifactExpectation};
        use nestweaver_store::ranking::{
            PAGERANK_ARTIFACT_KIND, PAGERANK_ARTIFACT_SCHEMA_VERSION, PAGERANK_DAMPING,
            PAGERANK_ITERATIONS, pagerank_algorithm_fingerprint, pagerank_declared_parameters,
        };

        let identity = nestweaver_store::PublicationIdentity {
            brain_uuid: "550e8400-e29b-41d4-a716-446655440000".to_string(),
            publication_uuid: "6ba7b810-9dad-11d1-80b4-00c04fd430c8".to_string(),
        };
        let scope = nestweaver_store::GraphScope::code_only();
        let scores: std::collections::HashMap<String, f64> =
            [("sym:a".to_string(), 1.0)].into_iter().collect();

        // Fully self-consistent by construction: the declaration and the
        // fingerprint are produced from the SAME parameters, so every check
        // internal to the artifact agrees.
        let sealed = |damping: f64, iterations: u32| -> Vec<u8> {
            let envelope = ArtifactEnvelope::new(
                ArtifactExpectation {
                    artifact_kind: PAGERANK_ARTIFACT_KIND,
                    artifact_schema_version: PAGERANK_ARTIFACT_SCHEMA_VERSION,
                    identity: &identity,
                    producer_version: env!("CARGO_PKG_VERSION"),
                    source_graph_generation: 1,
                    algorithm_fingerprint: &pagerank_algorithm_fingerprint(
                        damping, iterations, &scope,
                    ),
                },
                &scores,
            )
            .expect("seal artifact")
            .with_algorithm_parameters(pagerank_declared_parameters(damping, iterations, &scope));
            serde_json::to_vec(&envelope).expect("serialize envelope")
        };

        // The build's own parameters still load.
        pagerank_artifact_contract(
            &sealed(PAGERANK_DAMPING, PAGERANK_ITERATIONS),
            &identity,
            env!("CARGO_PKG_VERSION"),
            1,
        )
        .expect("an artifact matching this build's parameters must load");

        // A different damping, and a different iteration count, each refused
        // by name — despite being internally consistent.
        for (label, bytes) in [
            ("damping", sealed(0.5, PAGERANK_ITERATIONS)),
            (
                "iterations",
                sealed(PAGERANK_DAMPING, PAGERANK_ITERATIONS + 1),
            ),
        ] {
            let error = pagerank_artifact_contract(&bytes, &identity, env!("CARGO_PKG_VERSION"), 1)
                .expect_err("a foreign configuration must be refused");
            let rendered = format!("{error:#}");
            assert!(
                rendered.contains(label) && rendered.contains("this build computes with"),
                "{label} mismatch must be named: {rendered}"
            );
        }
    }

    /// nw-149. `validate_metadata` accepted three shapes it should not.
    ///
    /// Reported as a hardening asymmetry, NOT a proven escape — the probe
    /// matrix achieved no exploit. This closes gaps in an invariant; it does
    /// not patch a known attack, and the test is written to say so.
    #[test]
    fn bundle_metadata_rejects_empty_paths_aliases_and_empty_manifests() {
        let identity = nestweaver_store::PublicationIdentity {
            brain_uuid: "550e8400-e29b-41d4-a716-446655440000".to_string(),
            publication_uuid: "6ba7b810-9dad-11d1-80b4-00c04fd430c8".to_string(),
        };
        let descriptor = |path: &str| ArtifactDescriptor {
            path: path.to_string(),
            kind: ArtifactKind::Graph,
            artifact_schema_version: 1,
            byte_size: 1,
            blake3: "0".repeat(64),
            brain_uuid: identity.brain_uuid.clone(),
            publication_uuid: identity.publication_uuid.clone(),
            producer_version: env!("CARGO_PKG_VERSION").to_string(),
            source_graph_generation: 1,
            algorithm_fingerprint: "ladybugdb-graph-v1".to_string(),
        };
        let bundle_with = |artifacts: Vec<ArtifactDescriptor>| PublicationBundleV3 {
            format_version: crate::snapshot::SNAPSHOT_FORMAT_VERSION,
            brain_uuid: identity.brain_uuid.clone(),
            publication_uuid: identity.publication_uuid.clone(),
            producer_version: env!("CARGO_PKG_VERSION").to_string(),
            source_graph_generation: 1,
            artifacts,
        };
        let version = crate::snapshot::SNAPSHOT_FORMAT_VERSION;

        // A normal single-artifact manifest still validates — otherwise the
        // rejections below prove nothing.
        bundle_with(vec![descriptor("graph.lbug")])
            .validate_metadata(version)
            .expect("an ordinary manifest must still validate");

        // (1) The EMPTY path. `Path::new("")` is not absolute and yields no
        // components, so both existing guards pass it by construction — while
        // `validate_relative_path` in publication_operation.rs rejects it. Two
        // validators in one subsystem disagreeing about the same string.
        let error = bundle_with(vec![descriptor("")])
            .validate_metadata(version)
            .expect_err("an empty artifact path must be rejected");
        assert!(
            format!("{error:#}").contains("empty artifact path"),
            "{error:#}"
        );

        // (2) A manifest describing NOTHING. Previously caught only by
        // `resolve_selected_database` — after the pointer had moved.
        let error = bundle_with(vec![])
            .validate_metadata(version)
            .expect_err("a zero-artifact manifest must be rejected");
        assert!(format!("{error:#}").contains("no artifacts"), "{error:#}");

        // (3) Path ALIASES: distinct strings naming one file. A raw-string
        // duplicate check let the same artifact be described twice with
        // different metadata for each.
        for alias in ["a//b", "a/b/"] {
            let error = bundle_with(vec![descriptor("a/b"), descriptor(alias)])
                .validate_metadata(version)
                .unwrap_err();
            assert!(
                format!("{error:#}").contains("duplicate artifact"),
                "{alias} must collide with a/b: {error:#}"
            );
        }

        // (4) Case aliases. On a case-insensitive filesystem these are one
        // file, so a manifest must not rely on case to tell artifacts apart —
        // whether that works is a property of the READER's filesystem.
        let error = bundle_with(vec![descriptor("graph.lbug"), descriptor("GRAPH.LBUG")])
            .validate_metadata(version)
            .expect_err("case-only differences must be rejected");
        assert!(
            format!("{error:#}").contains("differing only by case"),
            "{error:#}"
        );

        // Genuinely distinct paths are still fine — the checks above must not
        // have collapsed every multi-artifact manifest.
        bundle_with(vec![descriptor("graph.lbug"), descriptor("pagerank.json")])
            .validate_metadata(version)
            .expect("distinct artifacts must still validate");
    }

    fn write_slot(root: &Path, identity: &nestweaver_store::PublicationIdentity) -> String {
        let slot = slot_path(root, &identity.publication_uuid).unwrap();
        std::fs::create_dir_all(&slot).unwrap();
        let bytes = serde_json::to_vec(&serde_json::json!({
            "brain_uuid": identity.brain_uuid,
            "publication_uuid": identity.publication_uuid,
        }))
        .unwrap();
        std::fs::write(slot.join(PUBLICATION_MANIFEST_FILE), &bytes).unwrap();
        crate::hash::blake3_hex_bytes(&bytes)
    }

    fn write_resolvable_slot(
        base_db: &Path,
        identity: &nestweaver_store::PublicationIdentity,
    ) -> CurrentPublicationPointer {
        let root = default_publication_root(base_db);
        let slot = slot_path(&root, &identity.publication_uuid).unwrap();
        std::fs::create_dir_all(&slot).unwrap();
        let graph_path = slot.join(PUBLICATION_GRAPH_FILE);
        let store =
            nestweaver_store::GraphStore::create_with_publication_identity(&graph_path, identity)
                .unwrap();
        let generation = store.graph_generation();
        drop(store);
        let graph = std::fs::read(&graph_path).unwrap();
        let bundle = PublicationBundleV3 {
            format_version: crate::snapshot::SNAPSHOT_FORMAT_VERSION,
            brain_uuid: identity.brain_uuid.clone(),
            publication_uuid: identity.publication_uuid.clone(),
            producer_version: env!("CARGO_PKG_VERSION").to_string(),
            source_graph_generation: generation,
            artifacts: vec![ArtifactDescriptor {
                path: PUBLICATION_GRAPH_FILE.to_string(),
                kind: ArtifactKind::Graph,
                artifact_schema_version: 1,
                byte_size: graph.len() as u64,
                blake3: crate::hash::blake3_hex_bytes(&graph),
                brain_uuid: identity.brain_uuid.clone(),
                publication_uuid: identity.publication_uuid.clone(),
                producer_version: env!("CARGO_PKG_VERSION").to_string(),
                source_graph_generation: generation,
                algorithm_fingerprint: "ladybugdb-graph-v1".to_string(),
            }],
        };
        let manifest = serde_json::to_vec_pretty(&bundle).unwrap();
        std::fs::write(slot.join(PUBLICATION_MANIFEST_FILE), &manifest).unwrap();
        let pointer = CurrentPublicationPointer::new(
            identity,
            None,
            crate::hash::blake3_hex_bytes(&manifest),
        )
        .unwrap();
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(
            current_pointer_path(&root),
            serde_json::to_vec_pretty(&pointer).unwrap(),
        )
        .unwrap();
        pointer
    }

    #[test]
    fn selected_database_resolution_is_fail_closed_and_keeps_legacy_default() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("brain.lbug");
        assert_eq!(resolve_selected_database(&base).unwrap(), base);

        let identity = nestweaver_store::PublicationIdentity::new_brain();
        let pointer = write_resolvable_slot(&base, &identity);
        let expected = slot_path(&default_publication_root(&base), &pointer.publication_uuid)
            .unwrap()
            .join(PUBLICATION_GRAPH_FILE);
        assert_eq!(resolve_selected_database(&base).unwrap(), expected);

        std::fs::write(&expected, b"not-a-database").unwrap();
        let error = resolve_selected_database(&base).unwrap_err().to_string();
        assert!(error.contains("open selected publication graph"), "{error}");
    }

    #[test]
    fn first_publication_rollback_returns_to_the_base_database() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("brain.lbug");
        let store = nestweaver_store::GraphStore::create(&base).unwrap();
        let incumbent = store.publication_identity().unwrap().unwrap();
        let target = incumbent.next_publication().unwrap();
        let pointer = write_resolvable_slot(&base, &target);
        let lease = store.acquire_index_publication_lease().unwrap();
        assert!(
            rollback_current(
                &default_publication_root(&base),
                &lease,
                &pointer.publication_uuid,
            )
            .unwrap()
            .is_none()
        );
        assert!(
            read_current(&default_publication_root(&base))
                .unwrap()
                .is_none()
        );
        assert_eq!(resolve_selected_database(&base).unwrap(), base);
        lease.release().unwrap();
    }

    #[test]
    fn rollback_under_an_existing_root_lock_does_not_reacquire_it() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("brain.lbug");
        let store = nestweaver_store::GraphStore::create(&base).unwrap();
        let incumbent = store.publication_identity().unwrap().unwrap();
        let target = incumbent.next_publication().unwrap();
        let pointer = write_resolvable_slot(&base, &target);
        let root = default_publication_root(&base);
        let root_lock = PublicationRootLock::acquire(&root).unwrap();
        let lease = store.acquire_index_publication_lease().unwrap();

        assert!(
            rollback_current_under_lock(&root, &lease, &pointer.publication_uuid, &root_lock,)
                .unwrap()
                .is_none()
        );
        assert!(read_current(&root).unwrap().is_none());
        lease.release().unwrap();
    }

    #[test]
    fn rollback_does_not_open_the_failed_selected_graph() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("brain.lbug");
        let store = nestweaver_store::GraphStore::create(&base).unwrap();
        let incumbent = store.publication_identity().unwrap().unwrap();
        let target = incumbent.next_publication().unwrap();
        let pointer = write_resolvable_slot(&base, &target);
        let target_graph = slot_path(&default_publication_root(&base), &target.publication_uuid)
            .unwrap()
            .join(PUBLICATION_GRAPH_FILE);
        std::fs::write(&target_graph, b"failed startup graph").unwrap();
        assert!(resolve_selected_database(&base).is_err());

        let lease = store.acquire_index_publication_lease().unwrap();
        assert!(
            rollback_current(
                &default_publication_root(&base),
                &lease,
                &pointer.publication_uuid,
            )
            .unwrap()
            .is_none()
        );
        assert_eq!(resolve_selected_database(&base).unwrap(), base);
        lease.release().unwrap();
    }

    #[test]
    fn rollback_is_one_step_and_never_reselects_the_abandoned_publication() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("brain.lbug");
        let base_store = nestweaver_store::GraphStore::create(&base).unwrap();
        let base_identity = base_store.publication_identity().unwrap().unwrap();
        drop(base_store);

        let first_identity = base_identity.next_publication().unwrap();
        let first = write_resolvable_slot(&base, &first_identity);
        let second_identity = first_identity.next_publication().unwrap();
        let second_without_predecessor = write_resolvable_slot(&base, &second_identity);
        let second = CurrentPublicationPointer::new(
            &second_identity,
            Some(first_identity.publication_uuid.clone()),
            second_without_predecessor.manifest_blake3,
        )
        .unwrap();
        let root = default_publication_root(&base);
        std::fs::write(
            current_pointer_path(&root),
            serde_json::to_vec_pretty(&second).unwrap(),
        )
        .unwrap();

        let predecessor = retained_predecessor_database(
            &base,
            second.expected_previous_publication_uuid.as_deref(),
        )
        .unwrap();
        let predecessor_store = nestweaver_store::GraphStore::open(&predecessor).unwrap();
        let lease = predecessor_store.acquire_index_publication_lease().unwrap();
        let selected = rollback_current(&root, &lease, &second.publication_uuid)
            .unwrap()
            .unwrap();
        assert_eq!(selected.publication_uuid, first.publication_uuid);
        assert_eq!(
            selected.rolled_back_from_publication_uuid.as_deref(),
            Some(second.publication_uuid.as_str())
        );
        assert!(selected.expected_previous_publication_uuid.is_none());

        let error = rollback_current(&root, &lease, &selected.publication_uuid).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("already represents the one-step rollback")
        );
        assert_eq!(
            read_current(&root).unwrap().unwrap().publication_uuid,
            first.publication_uuid,
            "a second rollback must not toggle back to the abandoned slot"
        );
        lease.release().unwrap();
    }

    /// nw-253: rollback must refuse a predecessor slot whose artifacts are
    /// symbolic links.
    ///
    /// Before the fix, `rollback_current_under_lock` read the predecessor's
    /// manifest, validated its metadata and identity, and went straight to
    /// `compare_and_swap_current` -- it never looked at the directory those
    /// bytes describe. So a graph artifact swapped for a link pointing OUTSIDE
    /// the slot was selected, and nothing downstream compensated:
    /// `resolve_selected_database` inspects the graph with `std::fs::metadata`,
    /// which follows links, so `is_file()` is true of the link.
    ///
    /// The predecessor is admitted under `ArtifactBytes::Live` (a selected
    /// graph stays writable after cutover), so the size/digest guard is
    /// deliberately NOT what fails here. Only the symlink refusal can.
    #[cfg(unix)]
    #[test]
    fn rollback_refuses_a_predecessor_slot_containing_a_symlinked_artifact() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("brain.lbug");
        let base_store = nestweaver_store::GraphStore::create(&base).unwrap();
        let base_identity = base_store.publication_identity().unwrap().unwrap();
        drop(base_store);

        let first_identity = base_identity.next_publication().unwrap();
        let first = write_resolvable_slot(&base, &first_identity);
        let second_identity = first_identity.next_publication().unwrap();
        let second_without_predecessor = write_resolvable_slot(&base, &second_identity);
        let second = CurrentPublicationPointer::new(
            &second_identity,
            Some(first_identity.publication_uuid.clone()),
            second_without_predecessor.manifest_blake3,
        )
        .unwrap();
        let root = default_publication_root(&base);
        std::fs::write(
            current_pointer_path(&root),
            serde_json::to_vec_pretty(&second).unwrap(),
        )
        .unwrap();

        // Move the predecessor's graph OUT of its slot and leave a link behind:
        // the bytes rollback is about to select now live wherever the link
        // points, which is precisely what a sealed slot promises cannot happen.
        let previous_slot = slot_path(&root, &first.publication_uuid).unwrap();
        let in_slot_graph = previous_slot.join(PUBLICATION_GRAPH_FILE);
        let outside = dir.path().join("outside-the-slot.lbug");
        std::fs::rename(&in_slot_graph, &outside).unwrap();
        std::os::unix::fs::symlink(&outside, &in_slot_graph).unwrap();

        let predecessor = retained_predecessor_database(
            &base,
            second.expected_previous_publication_uuid.as_deref(),
        )
        .unwrap();
        // Opening FOLLOWS the link, so the graph is perfectly usable and the
        // lease/identity checks all pass -- which is why nothing noticed.
        let predecessor_store = nestweaver_store::GraphStore::open(&predecessor).unwrap();
        let lease = predecessor_store.acquire_index_publication_lease().unwrap();

        let error = rollback_current(&root, &lease, &second.publication_uuid)
            .expect_err("rollback must refuse a predecessor slot containing a symbolic link");
        let message = error.to_string();
        assert!(message.contains("symbolic link"), "{message}");
        assert!(
            crate::publication_operation::PermanentPublicationFailure::is_permanent(&error),
            "a link in a sealed slot is not a transient condition: {message}"
        );
        assert_eq!(
            read_current(&root).unwrap().unwrap().publication_uuid,
            second.publication_uuid,
            "a refused rollback must leave CURRENT untouched"
        );
        lease.release().unwrap();
    }

    #[test]
    fn current_pointer_compare_and_swap_is_checked_and_durable() {
        let dir = tempfile::tempdir().unwrap();
        let store = nestweaver_store::GraphStore::in_memory().unwrap();
        let incumbent = store.publication_identity().unwrap().unwrap();
        let first_digest = write_slot(dir.path(), &incumbent);
        let first = CurrentPublicationPointer::new(&incumbent, None, first_digest).unwrap();
        let lease = store.acquire_index_publication_lease().unwrap();
        compare_and_swap_current(dir.path(), &lease, None, &first).unwrap();
        assert_eq!(read_current(dir.path()).unwrap(), Some(first.clone()));

        let next_identity = incumbent.next_publication().unwrap();
        let next_digest = write_slot(dir.path(), &next_identity);
        let next = CurrentPublicationPointer::new(
            &next_identity,
            Some(incumbent.publication_uuid.clone()),
            next_digest,
        )
        .unwrap();
        compare_and_swap_current(dir.path(), &lease, Some(&incumbent.publication_uuid), &next)
            .unwrap();
        assert_eq!(read_current(dir.path()).unwrap(), Some(next.clone()));

        let stale = incumbent.next_publication().unwrap();
        let stale_digest = write_slot(dir.path(), &stale);
        let stale_pointer = CurrentPublicationPointer::new(
            &stale,
            Some(incumbent.publication_uuid.clone()),
            stale_digest,
        )
        .unwrap();
        let error = compare_and_swap_current(
            dir.path(),
            &lease,
            Some(&incumbent.publication_uuid),
            &stale_pointer,
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("compare-and-swap conflict"), "{error}");
        assert_eq!(read_current(dir.path()).unwrap(), Some(next));
        lease.release().unwrap();
    }

    /// nw-135: nothing in the repo ever enumerated `slots/`. discard_operation
    /// refuses anything not cancelled-or-failed, so a slot from a SUCCESSFUL
    /// rebuild could never be removed in-tool — four rebuilds left four full
    /// slots on disk forever, ~1.2 GB each on the real brain.
    ///
    /// The dangerous direction is over-deletion, so this pins what is KEPT.
    /// Review finding: journals SURVIVE activation — only discard_operation
    /// removes one, and it refuses anything not cancelled-or-failed. Retaining
    /// every slot named by a surviving journal therefore pinned every slot ever
    /// published, and the prune reclaimed nothing at all on a real root, while
    /// its original test passed only because its fixture wrote slots with no
    /// journals.
    ///
    /// A slot whose operation is TERMINAL and which CURRENT no longer selects
    /// is exactly the superseded-slot leak nw-135 exists to close.
    /// Review finding: the previous guard was an `IndexPublicationLease`,
    /// which is an in-process Mutex/Condvar owned by ONE GraphStore — two
    /// processes, or two stores in one process, get unrelated leases, so it
    /// serialized nothing across processes. A database write lock cannot stand
    /// in either: activation locks the SELECTED slot graph while a prune
    /// inspects the base, so they lock different files.
    ///
    /// This proves the replacement is a real, root-anchored, cross-process
    /// lock: a SECOND acquisition of the same root is refused, including from
    /// another process.
    #[test]
    fn the_publication_root_lock_excludes_a_second_holder() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("brain.lbug.publications");

        let held = PublicationRootLock::acquire(&root).expect("first acquisition must succeed");
        let error = PublicationRootLock::acquire(&root)
            .expect_err("a second holder must be refused while the first lives");
        assert!(
            error.to_string().contains("another publication operation"),
            "the refusal must name the contention: {error}"
        );

        // Cross-PROCESS, through the production API rather than a platform
        // shell utility: exec a fresh copy of this test binary and have its
        // focused child test attempt the same canonical root.
        let probe = std::process::Command::new(std::env::current_exe().unwrap())
            .arg("publication::tests::publication_root_lock_child_probe")
            .arg("--exact")
            .arg("--nocapture")
            .env("NESTWEAVER_PUBLICATION_LOCK_PROBE_ROOT", &root)
            .output()
            .expect("run publication lock child probe");
        assert!(
            probe.status.success(),
            "another process must observe the production lock as held\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&probe.stdout),
            String::from_utf8_lossy(&probe.stderr)
        );
        assert!(
            String::from_utf8_lossy(&probe.stdout).contains("running 1 test"),
            "the exact child probe must actually run\nstdout:\n{}",
            String::from_utf8_lossy(&probe.stdout)
        );

        drop(held);
        PublicationRootLock::acquire(&root)
            .expect("the lock must be released when its guard drops");
    }

    #[test]
    fn publication_root_lock_child_probe() {
        let Some(root) = std::env::var_os("NESTWEAVER_PUBLICATION_LOCK_PROBE_ROOT") else {
            return;
        };
        let error = PublicationRootLock::acquire(Path::new(&root))
            .expect_err("the parent process must retain publication-root ownership");
        assert!(
            error.to_string().contains("another publication operation"),
            "the production API must report cross-process contention: {error}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn an_inherited_lock_description_does_not_outlive_publication_ownership() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("brain.lbug.publications");
        std::fs::create_dir(&root).unwrap();
        let path = root.join("LOCK");
        let owner = std::fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&path)
            .unwrap();
        lock_publication_file(&owner).unwrap();

        // A cloned descriptor shares the same open file description, exactly
        // as a forked child does before exec. Closing `owner` releases the
        // lock owner's description, so the clone keeps flock alive briefly.
        let inherited = owner.try_clone().unwrap();
        drop(owner);
        let child_window = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(50));
            drop(inherited);
        });

        let replacement = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        lock_publication_file(&replacement)
            .expect("a stale inherited flock must not manufacture live publication contention");
        child_window.join().unwrap();
    }

    #[test]
    fn publication_root_authorization_is_canonical_and_exact() {
        let relative_parent = tempfile::Builder::new()
            .prefix("publication-relative-")
            .tempdir_in(".")
            .unwrap();
        let relative_root = relative_parent
            .path()
            .strip_prefix(std::env::current_dir().unwrap())
            .unwrap()
            .join("publications");
        assert!(
            relative_root.is_relative(),
            "fixture must exercise a relative root"
        );
        std::fs::create_dir(&relative_root).unwrap();
        let lock = PublicationRootLock::acquire(&relative_root).unwrap();
        assert!(lock.authorizes(&relative_root));
        assert!(
            prune_slots(&relative_root, &lock, true)
                .unwrap()
                .slots
                .is_empty()
        );

        let unrelated = relative_parent.path().join("unrelated-publications");
        std::fs::create_dir(&unrelated).unwrap();
        assert!(!lock.authorizes(&unrelated));
        let error = prune_slots(&unrelated, &lock, true).unwrap_err();
        assert!(
            error.to_string().contains("does not authorize"),
            "wrong-root prune must fail closed: {error}"
        );
        let store = nestweaver_store::GraphStore::in_memory().unwrap();
        let lease = store.acquire_index_publication_lease().unwrap();
        let error = rollback_current_under_lock(&unrelated, &lease, "unused", &lock).unwrap_err();
        assert!(
            error.to_string().contains("does not authorize"),
            "wrong-root rollback must fail before inspecting the target: {error}"
        );
        lease.release().unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn publication_root_authorization_accepts_a_symlink_alias() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("publications");
        let alias = dir.path().join("publications-alias");
        std::fs::create_dir(&root).unwrap();
        std::os::unix::fs::symlink(&root, &alias).unwrap();

        let lock = PublicationRootLock::acquire(&alias).unwrap();
        assert!(lock.authorizes(&alias));
        assert!(lock.authorizes(&root));
        assert!(prune_slots(&alias, &lock, true).unwrap().slots.is_empty());
    }

    #[test]
    fn prune_reclaims_a_superseded_slot_whose_journal_survived_activation() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let store = nestweaver_store::GraphStore::in_memory().unwrap();
        let mut identity = store.publication_identity().unwrap().unwrap();

        // Three publications created the way the rebuild path creates them:
        // create_operation FIRST, then the slot.
        let mut uuids = Vec::new();
        for _ in 0..3 {
            let plan = crate::publication_operation::PublicationOperationPlan {
                operation_uuid: uuid::Uuid::new_v4().to_string(),
                brain_uuid: identity.brain_uuid.clone(),
                target_publication_uuid: identity.publication_uuid.clone(),
                expected_current_publication_uuid: None,
                input_fingerprint: "f".repeat(64),
                producer_version: "6.3.0".to_string(),
                publication_format_version: crate::snapshot::SNAPSHOT_FORMAT_VERSION,
                created_unix_millis: 1,
            };
            let created = crate::publication_operation::create_operation(root, plan).unwrap();
            let _ = write_slot(root, &identity);
            uuids.push((identity.clone(), created));
            identity = identity.next_publication().unwrap();
        }

        // An in-flight (non-terminal) journal must still pin its slot.
        let lock = PublicationRootLock::acquire(root).unwrap();
        let inflight = prune_slots(root, &lock, true).unwrap();
        assert_eq!(
            inflight.removed().count(),
            0,
            "operations still in flight must pin every slot they target"
        );

        // Drive all three to a terminal phase, as a successful publication does.
        for (_, created) in &uuids {
            let mut state =
                crate::publication_operation::load_operation(root, &created.plan.operation_uuid)
                    .unwrap();
            while !state.phase.is_terminal() {
                state = crate::publication_operation::request_cancel(
                    root,
                    &state.plan.operation_uuid,
                    state.revision,
                )
                .unwrap();
                state = crate::publication_operation::acknowledge_cancel(
                    root,
                    &state.plan.operation_uuid,
                    state.revision,
                )
                .unwrap();
            }
        }

        let report = prune_slots(root, &lock, false).unwrap();
        assert_eq!(
            report.removed().count(),
            3,
            "terminal journals must not pin superseded slots: {:?}",
            report
                .retained()
                .map(|s| s.retained_because.clone())
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn prune_keeps_current_its_predecessor_and_journal_targets() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let store = nestweaver_store::GraphStore::in_memory().unwrap();

        let predecessor = store.publication_identity().unwrap().unwrap();
        let predecessor_digest = write_slot(root, &predecessor);
        let current_identity = predecessor.next_publication().unwrap();
        let current_digest = write_slot(root, &current_identity);
        let orphan = current_identity.next_publication().unwrap();
        let _ = write_slot(root, &orphan);

        // Two cutovers, as the real flow performs them: the predecessor
        // becomes CURRENT first, then is superseded — which is what makes it
        // the retained rollback target.
        let lease = store.acquire_index_publication_lease().unwrap();
        let first = CurrentPublicationPointer::new(&predecessor, None, predecessor_digest).unwrap();
        compare_and_swap_current(root, &lease, None, &first).unwrap();
        let pointer = CurrentPublicationPointer::new(
            &current_identity,
            Some(predecessor.publication_uuid.clone()),
            current_digest,
        )
        .unwrap();
        compare_and_swap_current(
            root,
            &lease,
            Some(predecessor.publication_uuid.as_str()),
            &pointer,
        )
        .unwrap();
        drop(lease);

        // Dry run must report the orphan without touching the disk.
        let lock = PublicationRootLock::acquire(root).unwrap();
        let preview = prune_slots(root, &lock, true).unwrap();
        assert_eq!(preview.removed().count(), 1);
        assert!(
            slot_path(root, &orphan.publication_uuid).unwrap().exists(),
            "a dry run must not delete anything"
        );

        let report = prune_slots(root, &lock, false).unwrap();
        let removed: Vec<&str> = report
            .removed()
            .map(|slot| slot.publication_uuid.as_str())
            .collect();
        assert_eq!(removed, vec![orphan.publication_uuid.as_str()]);

        // The live graph and the one-step rollback target both survive.
        assert!(
            slot_path(root, &current_identity.publication_uuid)
                .unwrap()
                .exists(),
            "the slot CURRENT selects must never be reclaimed"
        );
        assert!(
            slot_path(root, &predecessor.publication_uuid)
                .unwrap()
                .exists(),
            "the retained predecessor backs the documented one-step rollback"
        );
        assert!(!slot_path(root, &orphan.publication_uuid).unwrap().exists());

        // Idempotent: a second pass finds nothing left to reclaim.
        assert_eq!(
            prune_slots(root, &lock, false).unwrap().removed().count(),
            0
        );
    }

    #[test]
    fn instance_anchor_inverts_slot_selection_and_leaves_other_paths_alone() {
        let base = Path::new("/data/brain.lbug");
        let uuid = "550e8400-e29b-41d4-a716-446655440000";
        let selected = slot_path(&default_publication_root(base), uuid)
            .expect("valid uuid")
            .join(PUBLICATION_GRAPH_FILE);

        // The inverse of resolve_selected_database: a selected slot graph
        // maps back to the base that names the brain.
        assert_eq!(instance_anchor_database(&selected), base);

        // A base path, and anything that is not a slot graph, is untouched.
        assert_eq!(instance_anchor_database(base), base);
        for other in [
            "/data/graph.lbug",                      // no slots/ ancestor
            "/data/brain.lbug.publications/slots/x", // not the graph file
            "/data/other/slots/x/graph.lbug",        // root lacks the suffix
        ] {
            assert_eq!(
                instance_anchor_database(Path::new(other)),
                Path::new(other),
                "unrelated path rewritten: {other}"
            );
        }
    }

    #[test]
    fn current_pointer_rejects_corruption_foreign_brains_and_unsealed_slots() {
        let dir = tempfile::tempdir().unwrap();
        let store = nestweaver_store::GraphStore::in_memory().unwrap();
        let incumbent = store.publication_identity().unwrap().unwrap();
        let digest = write_slot(dir.path(), &incumbent);
        let mut pointer = CurrentPublicationPointer::new(&incumbent, None, digest).unwrap();
        pointer.manifest_blake3 = "0".repeat(64);
        let error = pointer.validate().unwrap_err().to_string();
        assert!(error.contains("checksum mismatch"), "{error}");

        let foreign = nestweaver_store::PublicationIdentity::new_brain();
        let foreign_digest = write_slot(dir.path(), &foreign);
        let foreign_pointer =
            CurrentPublicationPointer::new(&foreign, None, foreign_digest).unwrap();
        let lease = store.acquire_index_publication_lease().unwrap();
        let error = compare_and_swap_current(dir.path(), &lease, None, &foreign_pointer)
            .unwrap_err()
            .to_string();
        assert!(error.contains("across brains"), "{error}");

        let next = incumbent.next_publication().unwrap();
        let missing_digest = "a".repeat(64);
        let missing = CurrentPublicationPointer::new(&next, None, missing_digest).unwrap();
        let error = compare_and_swap_current(dir.path(), &lease, None, &missing)
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("read target publication manifest"),
            "{error}"
        );
        assert!(read_current(dir.path()).unwrap().is_none());
        lease.release().unwrap();
    }
}
