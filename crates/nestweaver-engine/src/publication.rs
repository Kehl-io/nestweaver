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

/// Validate a live PageRank envelope before a sealed publication describes
/// it, returning the exact schema and algorithm/scope fingerprint carried by
/// the payload. This prevents snapshot and backup manifests from laundering a
/// foreign, stale, corrupt, or generically-labelled ranking sidecar.
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

/// Typed role of one file in a sealed publication bundle.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactKind {
    Graph,
    SourceManifest,
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
        let mut paths = std::collections::BTreeSet::new();
        for descriptor in &self.artifacts {
            let path = Path::new(&descriptor.path);
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
            if !paths.insert(descriptor.path.clone()) {
                anyhow::bail!(
                    "publication bundle contains duplicate artifact: {}",
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
    pub manifest_blake3: String,
    pub checksum_blake3: String,
}

#[derive(Serialize)]
struct PointerPayload<'a> {
    version: u32,
    brain_uuid: &'a str,
    publication_uuid: &'a str,
    expected_previous_publication_uuid: Option<&'a str>,
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
            manifest_blake3,
            checksum_blake3: String::new(),
        };
        pointer.checksum_blake3 = pointer.payload_digest()?;
        Ok(pointer)
    }

    fn payload_digest(&self) -> anyhow::Result<String> {
        let bytes = serde_json::to_vec(&PointerPayload {
            version: self.version,
            brain_uuid: &self.brain_uuid,
            publication_uuid: &self.publication_uuid,
            expected_previous_publication_uuid: self.expected_previous_publication_uuid.as_deref(),
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
        anyhow::bail!(
            "CURRENT compare-and-swap conflict: expected {}, observed {}",
            expected
                .map(|value| value.to_string())
                .unwrap_or_else(|| "<none>".to_string()),
            observed
                .map(|value| value.to_string())
                .unwrap_or_else(|| "<none>".to_string())
        );
    }
    let declared_previous = next
        .expected_previous_publication_uuid
        .as_deref()
        .map(|value| parse_uuid("declared previous publication_uuid", value))
        .transpose()?;
    if declared_previous != expected {
        anyhow::bail!(
            "CURRENT pointer declares previous {}, but compare-and-swap expects {}",
            declared_previous
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

use anyhow::Context;

#[cfg(test)]
mod tests {
    use super::*;

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
