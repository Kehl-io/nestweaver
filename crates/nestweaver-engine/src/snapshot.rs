use serde::{Deserialize, Serialize};
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::publication::{ArtifactDescriptor, ArtifactKind, PublicationBundleV3};

/// The oldest engine version that can read the current snapshot format.
///
/// Bump this ONLY when the snapshot layout changes in a backwards-incompatible
/// way (new required files, changed checksum format, etc.).  Routine engine
/// releases that don't touch the snapshot wire format should leave this alone.
/// Snapshot-format capability level implemented by this reader.
///
/// Format v2 made the embedding sidecar part of the authoritative snapshot.
/// Format v3 additionally binds the stamp to the graph's durable brain and
/// publication identities. Writers fence out older readers that cannot enforce
/// those invariants. The explicit capability version lets this development
/// tree read snapshots it writes before the package version is raised.
pub const MIN_SNAPSHOT_READER_VERSION: &str = "6.3.0";
pub const SNAPSHOT_FORMAT_VERSION: u32 = 3;
pub const SNAPSHOT_CAPABILITY_EMBEDDINGS: &str = "embedding-sidecar-v1";
pub const SNAPSHOT_CAPABILITY_PUBLICATION_IDENTITY: &str = "publication-identity-v1";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Stamp {
    #[serde(default)]
    pub format_version: u32,
    #[serde(default)]
    pub capabilities: Vec<String>,
    pub instance_id: String,
    /// Stable identity of the brain data. This is deliberately independent of
    /// the configured `instance_id` and the database's filesystem path.
    #[serde(default)]
    pub brain_uuid: String,
    /// Identity of the complete publication slot captured by this snapshot.
    #[serde(default)]
    pub publication_uuid: String,
    pub engine_version: String,
    pub min_compatible_engine: String,
    pub schema_hash_core: String,
    pub schema_hash_extensions: String,
    pub schema_hash_effective: String,
    pub embedding_model_id: String,
    pub embedding_dimension: u32,
    #[serde(default)]
    pub embedding_count: u64,
    pub built_at: String,
    pub repos: Vec<RepoStamp>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepoStamp {
    pub url: String,
    pub indexed_sha: String,
    pub commits_behind_head: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    pub repos: Vec<ManifestRepo>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManifestRepo {
    pub url: String,
    pub indexed_sha: String,
    pub files_skipped: Vec<SkippedFileEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkippedFileEntry {
    pub path: String,
    pub reason: String,
}

/// The filenames stored inside a snapshot directory.
const GRAPH_FILE: &str = "graph.lbug";
const MANIFEST_FILE: &str = "manifest.json";
const STAMP_FILE: &str = "stamp.json";
const CHECKSUM_FILE: &str = "checksum.blake3";
const PUBLICATION_FILE: &str = crate::publication::PUBLICATION_MANIFEST_FILE;
/// Sidecar filenames (relative to the db_path prefix, not snapshot_dir).
const SIDECAR_PAGERANK: &str = "pagerank.json";
const SIDECAR_MANIFESTS: &str = "manifests.json";
const SIDECAR_TANTIVY_DIR: &str = "tantivy";
const SIDECAR_REGEX_DIR: &str = "regex-v3";
const SIDECAR_EMBEDDINGS: &str = "embeddings.bin";

/// Core files that MUST be present and checksummed in every snapshot.
const CORE_FILES: &[&str] = &[GRAPH_FILE, MANIFEST_FILE, STAMP_FILE];

/// Sidecar files that are checksummed when present.
const SIDECAR_FILES: &[&str] = &[SIDECAR_PAGERANK, SIDECAR_MANIFESTS, SIDECAR_EMBEDDINGS];

/// Compute per-file BLAKE3 checksums for all core files and any present
/// sidecars.  Returns a checksum-style string: one `<hash>  <filename>\n`
/// line per file, sorted by filename for determinism.
fn compute_checksums(snapshot_dir: &Path) -> Result<String, anyhow::Error> {
    let mut lines: Vec<String> = Vec::new();
    for &name in CORE_FILES {
        let (_, hash) = crate::hash::blake3_file(snapshot_dir.join(name))
            .map_err(|e| anyhow::anyhow!("failed to stream {name} for checksum: {e}"))?;
        lines.push(format!("{hash}  {name}"));
    }
    for &name in SIDECAR_FILES {
        let path = snapshot_dir.join(name);
        if path.exists() {
            let (_, hash) = crate::hash::blake3_file(&path).map_err(|e| {
                anyhow::anyhow!("failed to stream sidecar {name} for checksum: {e}")
            })?;
            lines.push(format!("{hash}  {name}"));
        }
    }
    let publication = snapshot_dir.join(PUBLICATION_FILE);
    if publication.exists() {
        let (_, hash) = crate::hash::blake3_file(&publication).map_err(|e| {
            anyhow::anyhow!("failed to stream {PUBLICATION_FILE} for checksum: {e}")
        })?;
        lines.push(format!("{hash}  {PUBLICATION_FILE}"));
    }
    // Hash directory sidecars file-by-file in deterministic path order.
    for sidecar_dir in [SIDECAR_TANTIVY_DIR, SIDECAR_REGEX_DIR] {
        let directory = snapshot_dir.join(sidecar_dir);
        if !directory.is_dir() {
            continue;
        }
        let mut files = collect_files_recursive(&directory, sidecar_dir)?;
        files.sort();
        for (rel_path, abs_path) in files {
            let (_, hash) = crate::hash::blake3_file(&abs_path)
                .map_err(|e| anyhow::anyhow!("failed to stream {rel_path} for checksum: {e}"))?;
            lines.push(format!("{hash}  {rel_path}"));
        }
    }
    lines.sort();
    Ok(lines.join("\n") + "\n")
}

/// Recursively collect (relative_path, absolute_path) for all files under `dir`.
fn collect_files_recursive(
    dir: &Path,
    prefix: &str,
) -> Result<Vec<(String, PathBuf)>, anyhow::Error> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let ty = entry.file_type()?;
        let name = entry.file_name();
        let rel = format!("{prefix}/{}", name.to_string_lossy());
        if ty.is_dir() {
            out.extend(collect_files_recursive(&entry.path(), &rel)?);
        } else if ty.is_file() {
            out.push((rel, entry.path()));
        }
    }
    Ok(out)
}

/// Verify checksums from a checksum file.  Supports both the new per-file
/// format (multiple `<hash>  <filename>` lines) and the legacy single-hash
/// format for backwards compatibility.
fn verify_checksums(snapshot_dir: &Path) -> Result<(), anyhow::Error> {
    let checksum_path = snapshot_dir.join(CHECKSUM_FILE);
    // Fallback for pre-BLAKE3 snapshots that used checksum.sha256
    let checksum_path = if !checksum_path.exists() {
        let legacy = snapshot_dir.join("checksum.sha256");
        if legacy.exists() {
            tracing::warn!(
                "legacy checksum.sha256 found at {} — integrity verification is SKIPPED \
                 because these hashes use SHA-256 while current snapshots use BLAKE3. \
                 The snapshot will be re-created with BLAKE3 checksums on the next build.",
                legacy.display()
            );
            return Ok(());
        } else {
            checksum_path
        }
    } else {
        checksum_path
    };
    let stored = std::fs::read_to_string(&checksum_path)
        .map_err(|e| anyhow::anyhow!("failed to read {}: {e}", checksum_path.display()))?;
    let stored = stored.trim();

    if stored.contains("  ") {
        // New per-file format
        let mut referenced = std::collections::BTreeSet::new();
        for line in stored.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let (expected_hash, filename) = line
                .split_once("  ")
                .ok_or_else(|| anyhow::anyhow!("malformed checksum line: {line}"))?;
            let relative = Path::new(filename);
            if relative.is_absolute()
                || relative
                    .components()
                    .any(|component| !matches!(component, std::path::Component::Normal(_)))
            {
                anyhow::bail!("checksum contains unsafe artifact path: {filename}");
            }
            if !referenced.insert(filename.to_string()) {
                anyhow::bail!("checksum contains duplicate artifact: {filename}");
            }
            let file_path = snapshot_dir.join(filename);
            if !file_path.exists() {
                anyhow::bail!("checksum references missing file: {filename}");
            }
            let (_, actual) = crate::hash::blake3_file(&file_path)
                .map_err(|e| anyhow::anyhow!("failed to stream {filename}: {e}"))?;
            if actual != expected_hash {
                anyhow::bail!(
                    "integrity check failed for {filename}: \
                     expected {expected_hash}, got {actual}"
                );
            }
        }
        for required in CORE_FILES {
            if !referenced.contains(*required) {
                anyhow::bail!("checksum manifest omits required artifact: {required}");
            }
        }
        let mut actual = std::collections::BTreeSet::new();
        for (relative, _) in collect_files_recursive(snapshot_dir, "")? {
            let relative = relative.trim_start_matches('/');
            if relative != CHECKSUM_FILE && relative != "checksum.sha256" {
                actual.insert(relative.to_string());
            }
        }
        if actual != referenced {
            anyhow::bail!(
                "checksum manifest does not exactly cover snapshot artifacts (listed {}, present {})",
                referenced.len(),
                actual.len()
            );
        }
    } else {
        // Legacy single-hash format: concatenated hash of core files
        let mut hasher = blake3::Hasher::new();
        for name in CORE_FILES {
            let file = std::fs::File::open(snapshot_dir.join(name))
                .map_err(|e| anyhow::anyhow!("failed to open {name} for checksum: {e}"))?;
            crate::hash::update_blake3_stream(&mut hasher, file)
                .map_err(|e| anyhow::anyhow!("failed to stream {name} for checksum: {e}"))?;
        }
        let computed = hasher.finalize().to_hex().to_string();
        if computed != stored {
            anyhow::bail!(
                "snapshot integrity check failed: stored checksum does not match computed checksum"
            );
        }
    }
    Ok(())
}

use nestweaver_storage::copy_dir_all;

/// Build a snapshot in `output_dir`.
///
/// 1. Copy core files (graph, manifest, stamp).
/// 2. Copy sidecars (pagerank, manifests, tantivy) if present.
/// 3. Compute per-file BLAKE3 checksums over everything that was copied.
pub fn build_snapshot(
    output_dir: &Path,
    stamp: &Stamp,
    manifest: &Manifest,
    db_path: &Path,
) -> Result<Stamp, anyhow::Error> {
    let store = nestweaver_store::GraphStore::open(db_path)
        .map_err(|error| anyhow::anyhow!("failed to open snapshot database: {error}"))?;
    build_snapshot_from_store(output_dir, stamp, manifest, &store)
}

/// Build a snapshot from an already-open store while excluding graph
/// publishers for the complete checkpoint-and-copy lifetime.
pub fn build_snapshot_from_store(
    output_dir: &Path,
    stamp: &Stamp,
    manifest: &Manifest,
    store: &nestweaver_store::GraphStore,
) -> Result<Stamp, anyhow::Error> {
    let db_path = store
        .db_path()
        .ok_or_else(|| anyhow::anyhow!("cannot snapshot an in-memory graph store"))?;
    let publication = store
        .acquire_index_publication_lease()
        .map_err(|error| anyhow::anyhow!("failed to quiesce graph publication: {error}"))?;
    publication.ensure_clean_for_snapshot().map_err(|error| {
        anyhow::anyhow!("refusing snapshot of dirty index publication: {error}")
    })?;
    if output_dir.exists() {
        anyhow::bail!(
            "snapshot destination {} already exists; snapshots are immutable, choose a new destination",
            output_dir.display()
        );
    }
    let staging = sibling_staging_path(output_dir, "snapshot-build")?;
    std::fs::create_dir(&staging).map_err(|error| {
        anyhow::anyhow!(
            "failed to create snapshot staging dir {}: {error}",
            staging.display()
        )
    })?;

    let result = (|| {
        let embedding_lease = store
            .stage_embeddings_for_snapshot(&staging.join(SIDECAR_EMBEDDINGS))
            .map_err(|error| anyhow::anyhow!("failed to stage embeddings: {error}"))?;
        store
            .checkpoint()
            .map_err(|error| anyhow::anyhow!("snapshot CHECKPOINT failed: {error}"))?;

        let mut authoritative_stamp = stamp.clone();
        let embedding = embedding_lease.state();
        let publication_identity = store
            .publication_identity()
            .map_err(|error| anyhow::anyhow!("failed to read publication identity: {error}"))?
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "database has no publication identity; reopen it writable to initialize identity before snapshotting"
                )
            })?;
        authoritative_stamp.format_version = SNAPSHOT_FORMAT_VERSION;
        authoritative_stamp.capabilities = vec![
            SNAPSHOT_CAPABILITY_EMBEDDINGS.to_string(),
            SNAPSHOT_CAPABILITY_PUBLICATION_IDENTITY.to_string(),
        ];
        if !semver_ge(
            &authoritative_stamp.min_compatible_engine,
            MIN_SNAPSHOT_READER_VERSION,
        ) {
            authoritative_stamp.min_compatible_engine = MIN_SNAPSHOT_READER_VERSION.to_string();
        }
        authoritative_stamp.embedding_model_id = embedding.model_id.clone();
        authoritative_stamp.embedding_dimension = embedding.dimension;
        authoritative_stamp.embedding_count = embedding.count;
        authoritative_stamp.brain_uuid = publication_identity.brain_uuid;
        authoritative_stamp.publication_uuid = publication_identity.publication_uuid;
        build_snapshot_files(
            &staging,
            &authoritative_stamp,
            manifest,
            db_path,
            store.graph_generation(),
        )?;
        verify_snapshot(&staging).map_err(|error| {
            anyhow::anyhow!("staged snapshot failed publication validation: {error}")
        })?;
        sync_directory_tree(&staging)?;
        std::fs::rename(&staging, output_dir).map_err(|error| {
            anyhow::anyhow!(
                "failed to atomically publish snapshot {}: {error}",
                output_dir.display()
            )
        })?;
        nestweaver_store::durable_sidecar::sync_parent_directory_durable(output_dir)?;
        Ok(authoritative_stamp)
    })();

    if result.is_err() {
        let _ = std::fs::remove_dir_all(&staging);
    }
    let release = publication
        .release()
        .map_err(|error| anyhow::anyhow!("failed to release snapshot publication lease: {error}"));
    match (result, release) {
        (Ok(stamp), Ok(())) => Ok(stamp),
        (Err(error), _) => Err(error),
        (Ok(_), Err(error)) => Err(error),
    }
}

static STAGING_SEQUENCE: AtomicU64 = AtomicU64::new(0);

fn sibling_staging_path(destination: &Path, purpose: &str) -> Result<PathBuf, anyhow::Error> {
    let parent = destination
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent)?;
    let name = destination
        .file_name()
        .ok_or_else(|| anyhow::anyhow!("destination must have a file name"))?
        .to_string_lossy();
    let sequence = STAGING_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    Ok(parent.join(format!(
        ".{name}.{purpose}.{}.{}",
        std::process::id(),
        sequence
    )))
}

fn sync_directory_tree(root: &Path) -> Result<(), anyhow::Error> {
    for entry in std::fs::read_dir(root)? {
        let path = entry?.path();
        if path.is_dir() {
            sync_directory_tree(&path)?;
        } else {
            File::open(&path)?.sync_all()?;
        }
    }
    File::open(root)?.sync_all()?;
    Ok(())
}

/// Copy a rebuildable, generation-bound sidecar into a snapshot only when its
/// full identity/schema/fingerprint/payload contract is valid for the graph
/// being captured.
///
/// Generation staleness is the one recoverable case: the graph is healthy and
/// the optional artifact can be recomputed, so omit it with a warning. Every
/// other validation failure remains fatal rather than laundering a foreign or
/// corrupt artifact into an apparently valid snapshot.
fn copy_optional_generation_bound_sidecar(
    logical_path: &str,
    source: &Path,
    destination: &Path,
    stamp: &Stamp,
    source_graph_generation: u64,
) -> Result<bool, anyhow::Error> {
    match artifact_contract_for_path(logical_path, source, stamp, source_graph_generation) {
        Ok(_) => {}
        Err(error) => {
            let message = format!("{error:#}");
            if nestweaver_store::artifact_envelope::is_stale_artifact_generation(&message) {
                tracing::warn!(
                    artifact = logical_path,
                    source = %source.display(),
                    reason = %message,
                    "omitting generation-stale optional artifact from snapshot; rebuild it after restore"
                );
                return Ok(false);
            }
            return Err(anyhow::anyhow!(
                "snapshot refused optional artifact {logical_path} at {}: {message}",
                source.display()
            ));
        }
    }
    std::fs::copy(source, destination).map_err(|error| {
        anyhow::anyhow!(
            "failed to copy validated optional artifact {logical_path} from {}: {error}",
            source.display()
        )
    })?;
    Ok(true)
}

fn build_snapshot_files(
    output_dir: &Path,
    stamp: &Stamp,
    manifest: &Manifest,
    db_path: &Path,
    source_graph_generation: u64,
) -> Result<(), anyhow::Error> {
    // ── Core files ──────────────────────────────────────────────────────────
    std::fs::copy(db_path, output_dir.join(GRAPH_FILE))
        .map_err(|e| anyhow::anyhow!("failed to copy graph file: {e}"))?;

    let manifest_json = serde_json::to_string_pretty(manifest)?;
    std::fs::write(output_dir.join(MANIFEST_FILE), &manifest_json)?;

    let stamp_json = serde_json::to_string_pretty(stamp)?;
    std::fs::write(output_dir.join(STAMP_FILE), &stamp_json)?;

    // ── Sidecars ────────────────────────────────────────────────────────────
    let pagerank_src = crate::sidecar_path(db_path, &format!(".{SIDECAR_PAGERANK}"));
    if pagerank_src.exists() {
        copy_optional_generation_bound_sidecar(
            SIDECAR_PAGERANK,
            &pagerank_src,
            &output_dir.join(SIDECAR_PAGERANK),
            stamp,
            source_graph_generation,
        )?;
    } else {
        tracing::debug!(
            src = %pagerank_src.display(),
            "build_snapshot: pagerank sidecar not found, skipping"
        );
    }

    let manifests_src = crate::sidecar_path(db_path, &format!(".{SIDECAR_MANIFESTS}"));
    if manifests_src.exists() {
        copy_optional_generation_bound_sidecar(
            SIDECAR_MANIFESTS,
            &manifests_src,
            &output_dir.join(SIDECAR_MANIFESTS),
            stamp,
            source_graph_generation,
        )?;
    } else {
        tracing::debug!(
            src = %manifests_src.display(),
            "build_snapshot: manifests sidecar not found, skipping"
        );
    }

    let tantivy_src = crate::sidecar_path(db_path, ".tantivy");
    if tantivy_src.exists() && tantivy_src.is_dir() {
        copy_dir_all(&tantivy_src, &output_dir.join(SIDECAR_TANTIVY_DIR)).map_err(|e| {
            anyhow::anyhow!(
                "failed to copy tantivy directory {}: {e}",
                tantivy_src.display()
            )
        })?;
    } else {
        tracing::debug!(
            src = %tantivy_src.display(),
            "build_snapshot: tantivy index directory not found, skipping"
        );
    }

    let regex_src = crate::sidecar_path(db_path, ".regex-v3");
    if regex_src.is_dir() {
        copy_dir_all(&regex_src, &output_dir.join(SIDECAR_REGEX_DIR)).map_err(|error| {
            anyhow::anyhow!(
                "failed to copy regex-v3 directory {}: {error}",
                regex_src.display()
            )
        })?;
    } else {
        tracing::debug!(
            src = %regex_src.display(),
            "build_snapshot: regex-v3 index directory not found, skipping"
        );
    }

    write_publication_bundle(output_dir, stamp, source_graph_generation)?;

    // ── Checksums (after all files are in place) ────────────────────────────
    let checksums = compute_checksums(output_dir)?;
    std::fs::write(output_dir.join(CHECKSUM_FILE), &checksums)?;

    Ok(())
}

fn artifact_contract(path: &str, stamp: &Stamp) -> anyhow::Result<(ArtifactKind, u32, String)> {
    let contract = match path {
        GRAPH_FILE => (
            ArtifactKind::Graph,
            1,
            format!("ladybugdb:effective-schema:{}", stamp.schema_hash_effective),
        ),
        MANIFEST_FILE => (
            ArtifactKind::SourceManifest,
            1,
            "nestweaver-source-manifest-v1".to_string(),
        ),
        STAMP_FILE => (
            ArtifactKind::CompatibilityStamp,
            SNAPSHOT_FORMAT_VERSION,
            "nestweaver-snapshot-stamp-v3".to_string(),
        ),
        SIDECAR_PAGERANK => anyhow::bail!(
            "PageRank contract requires payload inspection; use artifact_contract_for_payload"
        ),
        SIDECAR_MANIFESTS => anyhow::bail!(
            "repository manifest contract requires payload inspection; use artifact_contract_for_payload"
        ),
        SIDECAR_EMBEDDINGS => anyhow::bail!(
            "embedding contract requires payload inspection; use artifact_contract_for_payload"
        ),
        path if path.starts_with(&format!("{SIDECAR_TANTIVY_DIR}/")) => (
            ArtifactKind::Bm25,
            1,
            "nestweaver-tantivy-bm25-v1".to_string(),
        ),
        path if path.starts_with(&format!("{SIDECAR_REGEX_DIR}/")) => (
            ArtifactKind::Regex,
            nestweaver_store::REGEX_INDEX_SCHEMA_VERSION,
            format!(
                "regex-v{}:{}",
                nestweaver_store::REGEX_INDEX_SCHEMA_VERSION,
                nestweaver_store::REGEX_TOKENIZER_FINGERPRINT
            ),
        ),
        _ => anyhow::bail!("unclassified publication artifact: {path}"),
    };
    Ok(contract)
}

fn artifact_contract_for_payload(
    path: &str,
    stamp: &Stamp,
    source_graph_generation: u64,
    payload: Option<&[u8]>,
) -> anyhow::Result<(ArtifactKind, u32, String)> {
    if path == SIDECAR_PAGERANK || path == SIDECAR_MANIFESTS || path == SIDECAR_EMBEDDINGS {
        let payload =
            payload.ok_or_else(|| anyhow::anyhow!("artifact contract requires its payload"))?;
        let identity = nestweaver_store::PublicationIdentity {
            brain_uuid: stamp.brain_uuid.clone(),
            publication_uuid: stamp.publication_uuid.clone(),
        };
        return if path == SIDECAR_PAGERANK {
            let (schema, fingerprint) = crate::publication::pagerank_artifact_contract(
                payload,
                &identity,
                &stamp.engine_version,
                source_graph_generation,
            )?;
            Ok((ArtifactKind::Ranking, schema, fingerprint))
        } else if path == SIDECAR_MANIFESTS {
            let (schema, fingerprint) = crate::publication::repo_manifest_artifact_contract(
                payload,
                &identity,
                &stamp.engine_version,
                source_graph_generation,
            )?;
            Ok((ArtifactKind::RepoManifest, schema, fingerprint))
        } else {
            let index = nestweaver_store::EmbeddingIndex::load_binary_v2_bytes(payload)
                .map_err(|error| anyhow::anyhow!("inspect embedding-v2 artifact: {error}"))?;
            let envelope = index
                .artifact_envelope()
                .ok_or_else(|| anyhow::anyhow!("embedding-v2 envelope is missing"))?;
            if envelope.brain_uuid != identity.brain_uuid
                || envelope.publication_uuid != identity.publication_uuid
                || envelope.source_graph_generation != source_graph_generation
            {
                anyhow::bail!("embedding-v2 identity or source generation mismatch");
            }
            Ok((
                ArtifactKind::Embeddings,
                envelope.schema_version,
                envelope.algorithm_fingerprint()?,
            ))
        };
    }
    artifact_contract(path, stamp)
}

fn artifact_contract_for_path(
    path: &str,
    absolute: &Path,
    stamp: &Stamp,
    source_graph_generation: u64,
) -> anyhow::Result<(ArtifactKind, u32, String)> {
    if path == SIDECAR_EMBEDDINGS {
        let identity = nestweaver_store::PublicationIdentity {
            brain_uuid: stamp.brain_uuid.clone(),
            publication_uuid: stamp.publication_uuid.clone(),
        };
        let index = nestweaver_store::EmbeddingIndex::load_binary_v2(absolute)
            .map_err(|error| anyhow::anyhow!("inspect embedding-v2 artifact: {error}"))?;
        let envelope = index
            .artifact_envelope()
            .ok_or_else(|| anyhow::anyhow!("embedding-v2 envelope is missing"))?;
        if envelope.brain_uuid != identity.brain_uuid
            || envelope.publication_uuid != identity.publication_uuid
            || envelope.source_graph_generation != source_graph_generation
        {
            anyhow::bail!("embedding-v2 identity or source generation mismatch");
        }
        return Ok((
            ArtifactKind::Embeddings,
            envelope.schema_version,
            envelope.algorithm_fingerprint()?,
        ));
    }
    let payload =
        if path == SIDECAR_PAGERANK || path == SIDECAR_MANIFESTS {
            Some(std::fs::read(absolute).map_err(|error| {
                anyhow::anyhow!("read self-describing artifact {path}: {error}")
            })?)
        } else {
            None
        };
    artifact_contract_for_payload(path, stamp, source_graph_generation, payload.as_deref())
}

fn write_publication_bundle(
    output_dir: &Path,
    stamp: &Stamp,
    source_graph_generation: u64,
) -> anyhow::Result<PublicationBundleV3> {
    let mut files = collect_files_recursive(output_dir, "")?;
    files.sort_by(|left, right| left.0.cmp(&right.0));
    let mut artifacts = Vec::with_capacity(files.len());
    for (path, absolute) in files {
        let path = path.trim_start_matches('/').to_string();
        if path == PUBLICATION_FILE || path == CHECKSUM_FILE || path == "checksum.sha256" {
            continue;
        }
        let (kind, artifact_schema_version, algorithm_fingerprint) =
            artifact_contract_for_path(&path, &absolute, stamp, source_graph_generation)?;
        let (byte_size, blake3) = crate::hash::blake3_file(&absolute)
            .map_err(|error| anyhow::anyhow!("stream publication artifact {path}: {error}"))?;
        artifacts.push(ArtifactDescriptor {
            path,
            kind,
            artifact_schema_version,
            byte_size,
            blake3,
            brain_uuid: stamp.brain_uuid.clone(),
            publication_uuid: stamp.publication_uuid.clone(),
            producer_version: stamp.engine_version.clone(),
            source_graph_generation,
            algorithm_fingerprint,
        });
    }
    let bundle = PublicationBundleV3 {
        format_version: SNAPSHOT_FORMAT_VERSION,
        brain_uuid: stamp.brain_uuid.clone(),
        publication_uuid: stamp.publication_uuid.clone(),
        producer_version: stamp.engine_version.clone(),
        source_graph_generation,
        artifacts,
    };
    let bytes = serde_json::to_vec_pretty(&bundle)?;
    std::fs::write(output_dir.join(PUBLICATION_FILE), bytes)?;
    Ok(bundle)
}

fn verify_publication_bundle(
    snapshot_dir: &Path,
    stamp: &Stamp,
    verify_payloads: bool,
) -> anyhow::Result<PublicationBundleV3> {
    let publication_path = snapshot_dir.join(PUBLICATION_FILE);
    let bytes = std::fs::read(&publication_path)
        .map_err(|error| anyhow::anyhow!("failed to read {PUBLICATION_FILE}: {error}"))?;
    let bundle: PublicationBundleV3 = serde_json::from_slice(&bytes)?;
    bundle.validate_metadata(SNAPSHOT_FORMAT_VERSION)?;
    if bundle.format_version != SNAPSHOT_FORMAT_VERSION {
        anyhow::bail!(
            "publication bundle format {} does not match expected v{SNAPSHOT_FORMAT_VERSION}",
            bundle.format_version
        );
    }
    let bundle_identity = nestweaver_store::PublicationIdentity {
        brain_uuid: bundle.brain_uuid.clone(),
        publication_uuid: bundle.publication_uuid.clone(),
    };
    bundle_identity
        .validate()
        .map_err(|error| anyhow::anyhow!("invalid publication bundle identity: {error}"))?;
    let stamp_identity = nestweaver_store::PublicationIdentity {
        brain_uuid: stamp.brain_uuid.clone(),
        publication_uuid: stamp.publication_uuid.clone(),
    };
    if bundle_identity != stamp_identity {
        anyhow::bail!(
            "publication bundle identity {}/{} does not match compatibility stamp {}/{}",
            bundle.brain_uuid,
            bundle.publication_uuid,
            stamp.brain_uuid,
            stamp.publication_uuid
        );
    }
    if bundle.producer_version != stamp.engine_version {
        anyhow::bail!(
            "publication bundle producer '{}' does not match stamp engine '{}'",
            bundle.producer_version,
            stamp.engine_version
        );
    }

    let mut described = std::collections::BTreeSet::new();
    for descriptor in &bundle.artifacts {
        let relative = Path::new(&descriptor.path);
        if relative.is_absolute()
            || relative
                .components()
                .any(|component| !matches!(component, std::path::Component::Normal(_)))
        {
            anyhow::bail!(
                "publication bundle contains unsafe artifact path: {}",
                descriptor.path
            );
        }
        if !described.insert(descriptor.path.clone()) {
            anyhow::bail!(
                "publication bundle contains duplicate artifact: {}",
                descriptor.path
            );
        }
        if descriptor.brain_uuid != bundle.brain_uuid
            || descriptor.publication_uuid != bundle.publication_uuid
        {
            anyhow::bail!(
                "publication artifact {} has foreign identity {}/{}",
                descriptor.path,
                descriptor.brain_uuid,
                descriptor.publication_uuid
            );
        }
        if descriptor.producer_version.is_empty()
            || descriptor.algorithm_fingerprint.is_empty()
            || descriptor.artifact_schema_version == 0
        {
            anyhow::bail!(
                "publication artifact {} has incomplete schema/producer/fingerprint metadata",
                descriptor.path
            );
        }
        if descriptor.source_graph_generation != bundle.source_graph_generation {
            anyhow::bail!(
                "publication artifact {} source generation {} does not match bundle {}",
                descriptor.path,
                descriptor.source_graph_generation,
                bundle.source_graph_generation
            );
        }
        let artifact = snapshot_dir.join(&descriptor.path);
        let (kind, schema_version, fingerprint) = artifact_contract_for_path(
            &descriptor.path,
            &artifact,
            stamp,
            bundle.source_graph_generation,
        )?;
        if descriptor.kind != kind
            || descriptor.artifact_schema_version != schema_version
            || descriptor.algorithm_fingerprint != fingerprint
        {
            anyhow::bail!(
                "publication artifact {} contract metadata does not match its declared path/kind",
                descriptor.path
            );
        }
        if verify_payloads {
            let (byte_size, digest) = crate::hash::blake3_file(&artifact).map_err(|error| {
                anyhow::anyhow!(
                    "failed to stream publication artifact {}: {error}",
                    descriptor.path
                )
            })?;
            if byte_size != descriptor.byte_size {
                anyhow::bail!(
                    "publication artifact {} size mismatch: descriptor {}, file {}",
                    descriptor.path,
                    descriptor.byte_size,
                    byte_size
                );
            }
            if digest != descriptor.blake3 {
                anyhow::bail!(
                    "publication artifact {} digest mismatch: descriptor {}, file {digest}",
                    descriptor.path,
                    descriptor.blake3
                );
            }
        }
    }

    let actual: std::collections::BTreeSet<String> = collect_files_recursive(snapshot_dir, "")?
        .into_iter()
        .map(|(path, _)| path.trim_start_matches('/').to_string())
        .filter(|path| {
            path != PUBLICATION_FILE && path != CHECKSUM_FILE && path != "checksum.sha256"
        })
        .collect();
    if actual != described {
        anyhow::bail!(
            "publication bundle does not exactly describe payloads (described {}, present {})",
            described.len(),
            actual.len()
        );
    }
    if bundle
        .artifacts
        .iter()
        .filter(|artifact| artifact.kind == ArtifactKind::Graph)
        .count()
        != 1
    {
        anyhow::bail!("publication bundle must contain exactly one graph artifact");
    }
    Ok(bundle)
}

pub fn publication_bundle_digest(snapshot_dir: &Path) -> anyhow::Result<String> {
    let stamp = verify_snapshot(snapshot_dir)?;
    let _ = verify_publication_bundle(snapshot_dir, &stamp, true)?;
    let (_, digest) = crate::hash::blake3_file(snapshot_dir.join(PUBLICATION_FILE))?;
    Ok(digest)
}

/// Verify a snapshot directory's integrity.
///
/// Supports both the new per-file checksum format and the legacy single-hash
/// format for backwards compatibility.
fn verify_snapshot_envelope(snapshot_dir: &Path) -> Result<Stamp, anyhow::Error> {
    verify_checksums(snapshot_dir)?;

    let stamp_json = std::fs::read_to_string(snapshot_dir.join(STAMP_FILE))
        .map_err(|e| anyhow::anyhow!("failed to read stamp.json: {e}"))?;
    let stamp: Stamp = serde_json::from_str(&stamp_json)?;

    if stamp.format_version > SNAPSHOT_FORMAT_VERSION {
        anyhow::bail!(
            "snapshot format {} is newer than this engine supports ({SNAPSHOT_FORMAT_VERSION})",
            stamp.format_version
        );
    }
    if stamp.format_version >= 3 {
        if !stamp
            .capabilities
            .iter()
            .any(|capability| capability == SNAPSHOT_CAPABILITY_PUBLICATION_IDENTITY)
        {
            anyhow::bail!(
                "snapshot format {} does not declare publication-identity capability",
                stamp.format_version
            );
        }
        let parse_identity = |name: &str, value: &str| {
            let parsed = uuid::Uuid::parse_str(value).map_err(|error| {
                anyhow::anyhow!("snapshot has invalid {name} '{value}': {error}")
            })?;
            if parsed.is_nil() {
                anyhow::bail!("snapshot has invalid {name}: nil UUID is not a data identity");
            }
            Ok(parsed)
        };
        let brain_uuid = parse_identity("brain_uuid", &stamp.brain_uuid)?;
        let publication_uuid = parse_identity("publication_uuid", &stamp.publication_uuid)?;
        if brain_uuid == publication_uuid {
            anyhow::bail!("snapshot brain_uuid and publication_uuid must be distinct");
        }
        let checksums = std::fs::read_to_string(snapshot_dir.join(CHECKSUM_FILE))?;
        if !checksums
            .lines()
            .any(|line| line.ends_with(&format!("  {PUBLICATION_FILE}")))
        {
            anyhow::bail!("snapshot v3 publication bundle is not covered by the checksum manifest");
        }
        verify_publication_bundle(snapshot_dir, &stamp, false)?;
    }
    let embedding_path = snapshot_dir.join(SIDECAR_EMBEDDINGS);
    if stamp.format_version == 0 {
        if (stamp.embedding_dimension > 0 || stamp.embedding_count > 0) && !embedding_path.exists()
        {
            anyhow::bail!(
                "legacy snapshot claims embeddings but omits embeddings.bin; rebuild the snapshot from the source database"
            );
        }
    } else {
        if !stamp
            .capabilities
            .iter()
            .any(|capability| capability == SNAPSHOT_CAPABILITY_EMBEDDINGS)
        {
            anyhow::bail!(
                "snapshot format {} does not declare embedding-sidecar capability",
                stamp.format_version
            );
        }
        if stamp.embedding_count > 0 && !embedding_path.exists() {
            anyhow::bail!(
                "snapshot contains {} embeddings but embeddings.bin is missing",
                stamp.embedding_count
            );
        }
        if stamp.embedding_count > 0 {
            let checksums = std::fs::read_to_string(snapshot_dir.join(CHECKSUM_FILE))?;
            if !checksums
                .lines()
                .any(|line| line.ends_with("  embeddings.bin"))
            {
                anyhow::bail!(
                    "snapshot embedding artifact is not covered by the checksum manifest"
                );
            }
        }
    }
    Ok(stamp)
}

fn verify_snapshot_artifacts(snapshot_dir: &Path, stamp: &Stamp) -> Result<(), anyhow::Error> {
    if stamp.format_version >= 3 {
        verify_publication_bundle(snapshot_dir, stamp, true)?;
        let graph_path = snapshot_dir.join(GRAPH_FILE);
        let graph = nestweaver_store::GraphStore::open_read_only(&graph_path)
            .map_err(|error| anyhow::anyhow!("open snapshot graph for identity check: {error}"))?;
        let graph_identity = graph
            .publication_identity()
            .map_err(|error| anyhow::anyhow!("read snapshot graph identity: {error}"))?
            .ok_or_else(|| anyhow::anyhow!("snapshot graph has no publication identity"))?;
        if graph_identity.brain_uuid != stamp.brain_uuid
            || graph_identity.publication_uuid != stamp.publication_uuid
        {
            anyhow::bail!(
                "snapshot publication identity mismatch: stamp brain/publication={}/{}, graph={}/{}",
                stamp.brain_uuid,
                stamp.publication_uuid,
                graph_identity.brain_uuid,
                graph_identity.publication_uuid
            );
        }
    }

    let embedding_path = snapshot_dir.join(SIDECAR_EMBEDDINGS);
    if embedding_path.exists() {
        let embeddings = nestweaver_store::EmbeddingIndex::load_binary_v2(&embedding_path)
            .map_err(|error| anyhow::anyhow!("invalid snapshot embedding artifact: {error}"))?;
        let count = u64::try_from(embeddings.len())?;
        let dimension = u32::try_from(embeddings.dimension().unwrap_or(0))?;
        if count != stamp.embedding_count || dimension != stamp.embedding_dimension {
            anyhow::bail!(
                "snapshot embedding metadata mismatch: stamp count/dimension={}/{}, artifact={}/{}",
                stamp.embedding_count,
                stamp.embedding_dimension,
                count,
                dimension
            );
        }
    }

    Ok(())
}

/// Verify a snapshot's checksummed envelope and then reopen its typed
/// artifacts to confirm that their embedded metadata agrees with the stamp.
pub fn verify_snapshot(snapshot_dir: &Path) -> Result<Stamp, anyhow::Error> {
    let stamp = verify_snapshot_envelope(snapshot_dir)?;
    verify_snapshot_artifacts(snapshot_dir, &stamp)?;
    Ok(stamp)
}

/// Compare two semver strings (e.g. "1.2.3"). Returns true if `a >= b`.
/// Only handles numeric major.minor.patch without pre-release suffixes.
fn semver_ge(a: &str, b: &str) -> bool {
    let parse = |s: &str| -> (u64, u64, u64) {
        let mut parts = s.trim().splitn(3, '.');
        let major = parts.next().and_then(|p| p.parse().ok()).unwrap_or(0);
        let minor = parts.next().and_then(|p| p.parse().ok()).unwrap_or(0);
        let patch = parts.next().and_then(|p| p.parse().ok()).unwrap_or(0);
        (major, minor, patch)
    };
    parse(a) >= parse(b)
}

/// Load a snapshot, validating compatibility against the running engine.
///
/// Steps:
/// 1. Verify the checksummed envelope without opening artifact payloads.
/// 2. Check `stamp.min_compatible_engine <= engine_version` (stamp requires at least that version).
/// 3. Check `stamp.schema_hash_effective == expected_schema_hash`.
/// 4. Check `stamp.embedding_model_id == expected_embedding_model`.
/// 5. Open and validate compatible artifact payloads.
/// 6. Return `(stamp, path to graph.lbug)`.
pub fn load_snapshot(
    snapshot_dir: &Path,
    engine_version: &str,
    expected_schema_hash: &str,
    expected_embedding_model: &str,
) -> Result<(Stamp, PathBuf), anyhow::Error> {
    load_snapshot_with_config(
        snapshot_dir,
        engine_version,
        Some(expected_schema_hash),
        Some(expected_embedding_model),
    )
}

/// Config-aware snapshot compatibility gate. Core snapshots can be loaded
/// without a config; extension snapshots require the caller's effective hash.
/// A supplied embedding model is an additional assertion, while absent config
/// trusts the verified snapshot/database fingerprint.
pub fn load_snapshot_with_config(
    snapshot_dir: &Path,
    engine_version: &str,
    expected_schema_hash: Option<&str>,
    expected_embedding_model: Option<&str>,
) -> Result<(Stamp, PathBuf), anyhow::Error> {
    let stamp = verify_snapshot_envelope(snapshot_dir)?;

    // The snapshot requires at least min_compatible_engine to load it.
    // If the running engine_version < min_compatible_engine, reject.
    // `engine_version` remains the application compatibility input. For v3,
    // this source tree has a reader capability newer than its package version;
    // using the explicit capability lets it read snapshots it writes while the
    // raised stamp still makes every pre-v3 reader fail closed.
    let reader_version = if engine_version == env!("CARGO_PKG_VERSION")
        && stamp.format_version >= 2
        && semver_ge(MIN_SNAPSHOT_READER_VERSION, &stamp.min_compatible_engine)
    {
        MIN_SNAPSHOT_READER_VERSION
    } else {
        engine_version
    };
    if !semver_ge(reader_version, &stamp.min_compatible_engine) {
        anyhow::bail!(
            "snapshot requires engine >= {} but current engine is {}; \
             rebuild the snapshot with a newer engine or downgrade the min_compatible_engine requirement",
            stamp.min_compatible_engine,
            engine_version
        );
    }

    let running_core = nestweaver_schema::core_schema_hash();
    if stamp.schema_hash_core != running_core {
        anyhow::bail!(
            "snapshot core schema hash mismatch: snapshot has '{}' but engine expects '{}'; rebuild the snapshot",
            stamp.schema_hash_core,
            running_core
        );
    }

    if stamp.schema_hash_extensions != "none" && expected_schema_hash.is_none() {
        anyhow::bail!("snapshot uses schema extensions and requires the matching instance config");
    }
    if let Some(expected_schema_hash) = expected_schema_hash
        && stamp.schema_hash_effective != expected_schema_hash
    {
        anyhow::bail!(
            "snapshot schema hash mismatch: snapshot has '{}' but engine expects '{}'; \
             rebuild the snapshot to pick up the new schema",
            stamp.schema_hash_effective,
            expected_schema_hash
        );
    }

    if let Some(expected_embedding_model) = expected_embedding_model
        && !stamp.embedding_model_id.is_empty()
        && stamp.embedding_model_id != expected_embedding_model
    {
        anyhow::bail!(
            "snapshot embedding model mismatch: snapshot used '{}' but engine expects '{}'; \
             rebuild the snapshot with the correct embedding model",
            stamp.embedding_model_id,
            expected_embedding_model
        );
    }

    // Only compatible artifacts are opened. This prevents an older reader
    // from asking LadybugDB or the embedding decoder to parse a payload whose
    // declared engine/schema contract it cannot understand.
    verify_snapshot_artifacts(snapshot_dir, &stamp)?;

    Ok((stamp, snapshot_dir.join(GRAPH_FILE)))
}

/// Materialize a verified snapshot into a private `working_dir`, laid out the way
/// the store expects (sidecars as `<db>.pagerank.json`, `<db>.manifests.json`,
/// `<db>.tantivy/`), and return the working graph DB path to open **read-only**.
///
/// The snapshot directory is never mutated. The compat gate runs FIRST via
/// [`load_snapshot`] (semver / schema / embedding) — if the snapshot is
/// incompatible this fails before copying anything, so a replica refuses to boot
/// on an incompatible artifact rather than serving wrong results.
pub fn materialize_snapshot(
    snapshot_dir: &Path,
    working_dir: &Path,
    engine_version: &str,
    expected_schema_hash: &str,
    expected_embedding_model: &str,
) -> Result<PathBuf, anyhow::Error> {
    materialize_snapshot_with_config(
        snapshot_dir,
        working_dir,
        engine_version,
        Some(expected_schema_hash),
        Some(expected_embedding_model),
    )
}

pub fn materialize_snapshot_with_config(
    snapshot_dir: &Path,
    working_dir: &Path,
    engine_version: &str,
    expected_schema_hash: Option<&str>,
    expected_embedding_model: Option<&str>,
) -> Result<PathBuf, anyhow::Error> {
    materialize_snapshot_with_config_and_hook(
        snapshot_dir,
        working_dir,
        engine_version,
        expected_schema_hash,
        expected_embedding_model,
        || {},
    )
}

fn materialize_snapshot_with_config_and_hook(
    snapshot_dir: &Path,
    working_dir: &Path,
    engine_version: &str,
    expected_schema_hash: Option<&str>,
    expected_embedding_model: Option<&str>,
    after_source_verification: impl FnOnce(),
) -> Result<PathBuf, anyhow::Error> {
    // Compat gate first — refuse an incompatible snapshot before touching disk.
    load_snapshot_with_config(
        snapshot_dir,
        engine_version,
        expected_schema_hash,
        expected_embedding_model,
    )?;
    after_source_verification();

    let staging = sibling_staging_path(working_dir, "snapshot-restore")?;
    if staging.exists() {
        std::fs::remove_dir_all(&staging)?;
    }
    std::fs::create_dir(&staging)?;
    let verified_snapshot = staging.join("verified-snapshot");
    let result = (|| {
        // Copy the snapshot representation first, then verify the copied bytes.
        // The source can change after the initial gate; only this staged,
        // checksum-verified copy is allowed to become the live replica.
        nestweaver_storage::copy_dir_all(snapshot_dir, &verified_snapshot)?;
        load_snapshot_with_config(
            &verified_snapshot,
            engine_version,
            expected_schema_hash,
            expected_embedding_model,
        )?;

        let db_path = staging.join(GRAPH_FILE);
        std::fs::rename(verified_snapshot.join(GRAPH_FILE), &db_path)
            .map_err(|e| anyhow::anyhow!("failed to stage verified snapshot graph file: {e}"))?;

        // Relocate JSON sidecars into the store's `<db><suffix>` convention.
        for (src_name, suffix) in [
            (SIDECAR_PAGERANK, ".pagerank.json"),
            (SIDECAR_MANIFESTS, ".manifests.json"),
            (SIDECAR_EMBEDDINGS, ".embeddings.bin"),
        ] {
            let src = verified_snapshot.join(src_name);
            if src.exists() {
                let dst = crate::sidecar_path(&db_path, suffix);
                std::fs::rename(&src, &dst).map_err(|e| {
                    anyhow::anyhow!("failed to stage verified sidecar {}: {e}", src.display())
                })?;
            }
        }

        // Relocate the Tantivy index directory into `<db>.tantivy`.
        let tantivy_src = verified_snapshot.join(SIDECAR_TANTIVY_DIR);
        if tantivy_src.is_dir() {
            let tantivy_dst = crate::sidecar_path(&db_path, ".tantivy");
            std::fs::rename(&tantivy_src, &tantivy_dst)
                .map_err(|e| anyhow::anyhow!("failed to stage verified tantivy index: {e}"))?;
        }
        let regex_src = verified_snapshot.join(SIDECAR_REGEX_DIR);
        if regex_src.is_dir() {
            let regex_dst = crate::sidecar_path(&db_path, ".regex-v3");
            std::fs::rename(&regex_src, &regex_dst)
                .map_err(|e| anyhow::anyhow!("failed to stage verified regex-v3 index: {e}"))?;
        }
        std::fs::remove_dir_all(&verified_snapshot)?;
        sync_directory_tree(&staging)?;
        publish_restored_directory(&staging, working_dir)?;
        Ok(working_dir.join(GRAPH_FILE))
    })();
    if result.is_err() {
        let _ = std::fs::remove_dir_all(&staging);
    }
    result
}

fn publish_restored_directory(staging: &Path, destination: &Path) -> Result<(), anyhow::Error> {
    if !destination.exists() {
        std::fs::rename(staging, destination)?;
        nestweaver_store::durable_sidecar::sync_parent_directory_durable(destination)?;
        return Ok(());
    }

    #[cfg(target_os = "linux")]
    {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;
        let old = CString::new(destination.as_os_str().as_bytes())?;
        let new = CString::new(staging.as_os_str().as_bytes())?;
        // RENAME_EXCHANGE is one namespace operation: readers see either the
        // complete old tree or the complete verified replacement.
        let result = unsafe {
            libc::syscall(
                libc::SYS_renameat2,
                libc::AT_FDCWD,
                new.as_ptr(),
                libc::AT_FDCWD,
                old.as_ptr(),
                libc::RENAME_EXCHANGE,
            )
        };
        if result != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        nestweaver_store::durable_sidecar::sync_parent_directory_durable(destination)?;
        std::fs::remove_dir_all(staging)?;
        nestweaver_store::durable_sidecar::sync_parent_directory_durable(destination)?;
        Ok(())
    }

    #[cfg(not(target_os = "linux"))]
    anyhow::bail!(
        "atomic replacement of an existing replica directory is unsupported on this platform; remove {} while the replica is stopped and retry",
        destination.display()
    )
}

/// Compute `(core, extensions, effective)` schema hashes for `cfg`. This MUST
/// match the values a snapshot's stamp is built with so a replica's compat gate
/// agrees — both the `snapshot build` CLI and a replica boot call this.
pub fn schema_hashes(cfg: Option<&crate::config::InstanceConfig>) -> (String, String, String) {
    let core = nestweaver_schema::core_schema_hash();
    let ext = match cfg.and_then(|c| c.schema_extensions.as_ref()) {
        Some(ext) => {
            let mut parts: Vec<String> = Vec::new();
            if let Some(ref props) = ext.extra_node_properties {
                let mut labels: Vec<&String> = props.keys().collect();
                labels.sort();
                for label in labels {
                    let inner = &props[label];
                    let mut keys: Vec<&String> = inner.keys().collect();
                    keys.sort();
                    for key in keys {
                        parts.push(format!("{label}.{key}={}", inner[key]));
                    }
                }
            }
            let joined = parts.join("\n");
            use sha2::Digest;
            sha2::Sha256::digest(joined.as_bytes())
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>()
        }
        None => "none".to_string(),
    };
    let effective = nestweaver_schema::effective_schema_hash(&core, &ext);
    (core, ext, effective)
}

// ── tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// A representative numeric reader version below the v3 capability floor.
    /// It must not equal this development tree's package version: matching the
    /// package version deliberately activates the source reader's newer
    /// capability override before the release version itself is raised.
    const PRE_V3_READER: &str = "6.2.999";

    fn make_stamp(
        engine_version: &str,
        min_compatible: &str,
        schema_hash: &str,
        model: &str,
    ) -> Stamp {
        Stamp {
            format_version: 0,
            capabilities: Vec::new(),
            instance_id: "test-instance".to_string(),
            brain_uuid: String::new(),
            publication_uuid: String::new(),
            engine_version: engine_version.to_string(),
            min_compatible_engine: min_compatible.to_string(),
            schema_hash_core: nestweaver_schema::core_schema_hash(),
            schema_hash_extensions: "none".to_string(),
            schema_hash_effective: schema_hash.to_string(),
            embedding_model_id: model.to_string(),
            embedding_dimension: 1536,
            embedding_count: 0,
            built_at: "2026-05-22T00:00:00Z".to_string(),
            repos: vec![RepoStamp {
                url: "https://github.com/example/repo".to_string(),
                indexed_sha: "abc123".to_string(),
                commits_behind_head: 0,
            }],
        }
    }

    fn make_manifest() -> Manifest {
        Manifest {
            repos: vec![ManifestRepo {
                url: "https://github.com/example/repo".to_string(),
                indexed_sha: "abc123".to_string(),
                files_skipped: vec![],
            }],
        }
    }

    fn make_test_db(dir: &Path) -> PathBuf {
        let db = dir.join("test.lbug");
        drop(nestweaver_store::GraphStore::create(&db).unwrap());
        db
    }

    #[test]
    fn build_creates_all_files() {
        let dir = tempfile::tempdir().unwrap();
        let snap_dir = dir.path().join("snapshot");
        let db = make_test_db(dir.path());

        let stamp = make_stamp(
            "0.1.0",
            "0.1.0",
            "schema-hash-abc",
            "text-embedding-3-small",
        );
        let manifest = make_manifest();

        let built = build_snapshot(&snap_dir, &stamp, &manifest, &db).unwrap();

        assert!(snap_dir.join(GRAPH_FILE).exists(), "graph.lbug missing");
        assert!(
            snap_dir.join(MANIFEST_FILE).exists(),
            "manifest.json missing"
        );
        assert!(snap_dir.join(STAMP_FILE).exists(), "stamp.json missing");
        assert!(
            snap_dir.join(PUBLICATION_FILE).exists(),
            "publication.json missing"
        );
        assert!(
            snap_dir.join(CHECKSUM_FILE).exists(),
            "checksum.blake3 missing"
        );
        let source_identity = nestweaver_store::GraphStore::open_read_only(&db)
            .unwrap()
            .publication_identity()
            .unwrap()
            .unwrap();
        assert_eq!(built.brain_uuid, source_identity.brain_uuid);
        assert_eq!(built.publication_uuid, source_identity.publication_uuid);
        assert!(
            built
                .capabilities
                .iter()
                .any(|capability| capability == SNAPSHOT_CAPABILITY_PUBLICATION_IDENTITY)
        );

        let bundle: PublicationBundleV3 =
            serde_json::from_slice(&std::fs::read(snap_dir.join(PUBLICATION_FILE)).unwrap())
                .unwrap();
        assert_eq!(bundle.brain_uuid, source_identity.brain_uuid);
        assert_eq!(bundle.publication_uuid, source_identity.publication_uuid);
        assert_eq!(
            bundle
                .artifacts
                .iter()
                .filter(|artifact| artifact.kind == ArtifactKind::Graph)
                .count(),
            1
        );
        assert!(
            bundle
                .artifacts
                .iter()
                .all(|artifact| artifact.brain_uuid == source_identity.brain_uuid
                    && artifact.publication_uuid == source_identity.publication_uuid
                    && !artifact.algorithm_fingerprint.is_empty())
        );
        let digest = publication_bundle_digest(&snap_dir).unwrap();
        assert_eq!(
            digest,
            crate::hash::blake3_hex_bytes(&std::fs::read(snap_dir.join(PUBLICATION_FILE)).unwrap())
        );
    }

    #[test]
    fn v3_bundle_rejects_foreign_artifact_identity_even_when_rechecksummed() {
        let dir = tempfile::tempdir().unwrap();
        let snap_dir = dir.path().join("snapshot");
        let db = make_test_db(dir.path());
        build_snapshot(
            &snap_dir,
            &make_stamp("0.1.0", "0.1.0", "schema-hash", "model"),
            &make_manifest(),
            &db,
        )
        .unwrap();

        let publication_path = snap_dir.join(PUBLICATION_FILE);
        let mut bundle: PublicationBundleV3 =
            serde_json::from_slice(&std::fs::read(&publication_path).unwrap()).unwrap();
        bundle.artifacts[0].brain_uuid = uuid::Uuid::new_v4().to_string();
        std::fs::write(
            &publication_path,
            serde_json::to_vec_pretty(&bundle).unwrap(),
        )
        .unwrap();
        std::fs::write(
            snap_dir.join(CHECKSUM_FILE),
            compute_checksums(&snap_dir).unwrap(),
        )
        .unwrap();

        let error = verify_snapshot(&snap_dir).unwrap_err().to_string();
        assert!(error.contains("foreign identity"), "{error}");
    }

    #[test]
    fn build_rejects_abandoned_dirty_publication_without_copying_marker() {
        let dir = tempfile::tempdir().unwrap();
        let snap_dir = dir.path().join("snapshot");
        let db = make_test_db(dir.path());
        let marker = crate::sidecar_path(&db, ".index-dirty");
        let store = nestweaver_store::GraphStore::open(&db).unwrap();
        let abandoned = crate::index::establish_index_publication_marker_with_io(
            &store,
            Some(&db),
            "abandoned snapshot publication",
            &crate::index::FileSystemIndexEpilogueIo,
        )
        .unwrap();
        drop(abandoned);

        let error = build_snapshot_from_store(
            &snap_dir,
            &make_stamp("0.1.0", "0.1.0", "schema-hash", "model"),
            &make_manifest(),
            &store,
        )
        .unwrap_err();
        assert!(error.to_string().contains("dirty index publication"));
        assert!(
            marker.exists(),
            "rejected snapshot must retain dirty marker"
        );
        assert!(
            !snap_dir.join("index-dirty").exists(),
            "dirty marker must never be included in snapshot output"
        );
    }

    #[test]
    fn verify_passes_valid_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let snap_dir = dir.path().join("snapshot");
        let db = make_test_db(dir.path());

        let stamp = make_stamp(
            "0.1.0",
            "0.1.0",
            "schema-hash-abc",
            "text-embedding-3-small",
        );
        let manifest = make_manifest();
        build_snapshot(&snap_dir, &stamp, &manifest, &db).unwrap();

        let loaded = verify_snapshot(&snap_dir).unwrap();
        assert_eq!(loaded.instance_id, "test-instance");
        assert_eq!(loaded.schema_hash_effective, "schema-hash-abc");
    }

    #[test]
    fn materialize_relocates_sidecars_and_gates_compat() {
        let dir = tempfile::tempdir().unwrap();
        let snap_dir = dir.path().join("snapshot");
        let db = make_test_db(dir.path());
        // Give the source DB sidecars so build_snapshot captures them.
        let store = nestweaver_store::GraphStore::open(&db).unwrap();
        store
            .compute_pagerank(
                0.85,
                20,
                &nestweaver_store::ranking::GraphScope::code_only(),
            )
            .unwrap();
        store
            .save_pagerank_cache(&crate::sidecar_path(&db, ".pagerank.json"))
            .unwrap();
        crate::save_manifest_cache_for_db(&std::collections::HashMap::new(), &store, &db).unwrap();
        drop(store);

        let stamp = make_stamp(
            env!("CARGO_PKG_VERSION"),
            "0.1.0",
            "schema-hash-abc",
            "text-embedding-3-small",
        );
        let manifest = make_manifest();
        build_snapshot(&snap_dir, &stamp, &manifest, &db).unwrap();

        // Compatible args → materialize into a private working dir with the
        // store's `<db><suffix>` sidecar layout.
        let work = dir.path().join("work");
        let db_path = materialize_snapshot(
            &snap_dir,
            &work,
            env!("CARGO_PKG_VERSION"),
            "schema-hash-abc",
            "text-embedding-3-small",
        )
        .unwrap();
        assert!(db_path.exists(), "working graph db must exist");
        assert!(
            crate::sidecar_path(&db_path, ".pagerank.json").exists(),
            "pagerank sidecar must be relocated to <db>.pagerank.json"
        );
        assert!(
            crate::sidecar_path(&db_path, ".manifests.json").exists(),
            "manifests sidecar must be relocated"
        );
        // The source snapshot dir is never mutated.
        assert!(snap_dir.join(GRAPH_FILE).exists());

        // Incompatible schema hash → refuse (compat gate runs before any copy).
        let work2 = dir.path().join("work2");
        assert!(
            materialize_snapshot(
                &snap_dir,
                &work2,
                env!("CARGO_PKG_VERSION"),
                "WRONG-schema-hash",
                "text-embedding-3-small",
            )
            .is_err(),
            "an incompatible snapshot must refuse to materialize"
        );
    }

    #[test]
    fn build_omits_only_generation_stale_optional_derived_artifacts() {
        let dir = tempfile::tempdir().unwrap();
        let snap_dir = dir.path().join("snapshot");
        let db = make_test_db(dir.path());
        let store = nestweaver_store::GraphStore::open(&db).unwrap();
        store
            .compute_pagerank(
                nestweaver_store::ranking::PAGERANK_DAMPING,
                nestweaver_store::ranking::PAGERANK_ITERATIONS,
                &nestweaver_store::ranking::GraphScope::code_only(),
            )
            .unwrap();
        store
            .save_pagerank_cache(&crate::sidecar_path(&db, ".pagerank.json"))
            .unwrap();
        crate::save_manifest_cache_for_db(&std::collections::HashMap::new(), &store, &db).unwrap();
        store.bump_and_persist_graph_generation(&crate::sidecar_path(&db, ".generation"));
        drop(store);

        build_snapshot(
            &snap_dir,
            &make_stamp(env!("CARGO_PKG_VERSION"), "0.1.0", "schema-hash", "model"),
            &make_manifest(),
            &db,
        )
        .unwrap();

        assert!(!snap_dir.join(SIDECAR_PAGERANK).exists());
        assert!(!snap_dir.join(SIDECAR_MANIFESTS).exists());
        verify_snapshot(&snap_dir).unwrap();
    }

    #[test]
    fn build_refuses_corrupt_optional_derived_artifact_instead_of_omitting_it() {
        let dir = tempfile::tempdir().unwrap();
        let snap_dir = dir.path().join("snapshot");
        let db = make_test_db(dir.path());
        let pagerank_path = crate::sidecar_path(&db, ".pagerank.json");
        std::fs::write(&pagerank_path, b"not an artifact envelope").unwrap();

        let error = build_snapshot(
            &snap_dir,
            &make_stamp(env!("CARGO_PKG_VERSION"), "0.1.0", "schema-hash", "model"),
            &make_manifest(),
            &db,
        )
        .unwrap_err()
        .to_string();

        assert!(error.contains("snapshot refused optional artifact pagerank.json"));
        assert!(!snap_dir.exists(), "failed snapshot must not be published");
    }

    #[test]
    fn build_refuses_foreign_optional_derived_artifact_instead_of_omitting_it() {
        let dir = tempfile::tempdir().unwrap();
        let foreign_dir = tempfile::tempdir().unwrap();
        let snap_dir = dir.path().join("snapshot");
        let db = make_test_db(dir.path());
        let foreign_db = make_test_db(foreign_dir.path());
        let foreign = nestweaver_store::GraphStore::open(&foreign_db).unwrap();
        foreign
            .compute_pagerank(
                nestweaver_store::ranking::PAGERANK_DAMPING,
                nestweaver_store::ranking::PAGERANK_ITERATIONS,
                &nestweaver_store::ranking::GraphScope::code_only(),
            )
            .unwrap();
        let foreign_pagerank = crate::sidecar_path(&foreign_db, ".pagerank.json");
        foreign.save_pagerank_cache(&foreign_pagerank).unwrap();
        std::fs::copy(
            &foreign_pagerank,
            crate::sidecar_path(&db, ".pagerank.json"),
        )
        .unwrap();

        let error = build_snapshot(
            &snap_dir,
            &make_stamp(env!("CARGO_PKG_VERSION"), "0.1.0", "schema-hash", "model"),
            &make_manifest(),
            &db,
        )
        .unwrap_err()
        .to_string();

        assert!(error.contains("foreign artifact identity"), "{error}");
        assert!(!snap_dir.exists(), "failed snapshot must not be published");
    }

    #[test]
    fn optional_derived_artifact_schema_and_fingerprint_mismatch_remain_fatal() {
        let dir = tempfile::tempdir().unwrap();
        let db = make_test_db(dir.path());
        let store = nestweaver_store::GraphStore::open(&db).unwrap();
        store
            .compute_pagerank(
                nestweaver_store::ranking::PAGERANK_DAMPING,
                nestweaver_store::ranking::PAGERANK_ITERATIONS,
                &nestweaver_store::ranking::GraphScope::code_only(),
            )
            .unwrap();
        let pagerank_path = crate::sidecar_path(&db, ".pagerank.json");
        store.save_pagerank_cache(&pagerank_path).unwrap();
        let identity = store.publication_identity().unwrap().unwrap();
        let mut stamp = make_stamp(env!("CARGO_PKG_VERSION"), "0.1.0", "schema-hash", "model");
        stamp.brain_uuid = identity.brain_uuid;
        stamp.publication_uuid = identity.publication_uuid;
        let valid: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&pagerank_path).unwrap()).unwrap();

        for (field, replacement, expected) in [
            (
                "artifact_schema_version",
                serde_json::json!(nestweaver_store::ranking::PAGERANK_ARTIFACT_SCHEMA_VERSION + 1),
                "incompatible artifact",
            ),
            (
                "algorithm_fingerprint",
                serde_json::json!("foreign-fingerprint"),
                "fingerprint",
            ),
        ] {
            let mut mismatched = valid.clone();
            mismatched[field] = replacement;
            std::fs::write(&pagerank_path, serde_json::to_vec(&mismatched).unwrap()).unwrap();
            let error = copy_optional_generation_bound_sidecar(
                SIDECAR_PAGERANK,
                &pagerank_path,
                &dir.path().join(format!("copied-{field}.json")),
                &stamp,
                store.graph_generation(),
            )
            .unwrap_err()
            .to_string();
            assert!(error.contains(expected), "{field}: {error}");
            assert!(
                !nestweaver_store::artifact_envelope::is_stale_artifact_generation(&error),
                "{field} mismatch must not be misclassified as rebuildable staleness"
            );
        }
    }

    #[test]
    fn verify_fails_tampered_stamp() {
        let dir = tempfile::tempdir().unwrap();
        let snap_dir = dir.path().join("snapshot");
        let db = make_test_db(dir.path());

        let stamp = make_stamp(
            "0.1.0",
            "0.1.0",
            "schema-hash-abc",
            "text-embedding-3-small",
        );
        let manifest = make_manifest();
        build_snapshot(&snap_dir, &stamp, &manifest, &db).unwrap();

        // Tamper with stamp.json
        std::fs::write(
            snap_dir.join(STAMP_FILE),
            r#"{"instance_id":"tampered","engine_version":"0.1.0","min_compatible_engine":"0.1.0","schema_hash_core":"x","schema_hash_extensions":"x","schema_hash_effective":"evil-hash","embedding_model_id":"bad","embedding_dimension":0,"built_at":"","repos":[]}"#,
        )
        .unwrap();

        let result = verify_snapshot(&snap_dir);
        assert!(
            result.is_err(),
            "tampered snapshot should fail verification"
        );
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("integrity check failed") || msg.contains("checksum"),
            "unexpected error: {msg}"
        );
    }

    #[test]
    fn verify_rejects_stamp_identity_that_does_not_match_graph() {
        let dir = tempfile::tempdir().unwrap();
        let snap_dir = dir.path().join("snapshot");
        let db = make_test_db(dir.path());
        build_snapshot(
            &snap_dir,
            &make_stamp("0.1.0", "0.1.0", "schema-hash-abc", "model"),
            &make_manifest(),
            &db,
        )
        .unwrap();

        let stamp_path = snap_dir.join(STAMP_FILE);
        let mut stamp: Stamp =
            serde_json::from_str(&std::fs::read_to_string(&stamp_path).unwrap()).unwrap();
        stamp.publication_uuid = uuid::Uuid::new_v4().to_string();
        std::fs::write(&stamp_path, serde_json::to_string_pretty(&stamp).unwrap()).unwrap();
        std::fs::write(
            snap_dir.join(CHECKSUM_FILE),
            compute_checksums(&snap_dir).unwrap(),
        )
        .unwrap();

        let error = verify_snapshot(&snap_dir).unwrap_err().to_string();
        assert!(
            error.contains("publication bundle identity")
                || error.contains("publication identity mismatch"),
            "{error}"
        );
    }

    #[test]
    fn verify_rejects_equivalent_identity_encodings() {
        let dir = tempfile::tempdir().unwrap();
        let snap_dir = dir.path().join("snapshot");
        let db = make_test_db(dir.path());
        build_snapshot(
            &snap_dir,
            &make_stamp("0.1.0", "0.1.0", "schema-hash-abc", "model"),
            &make_manifest(),
            &db,
        )
        .unwrap();

        let stamp_path = snap_dir.join(STAMP_FILE);
        let mut stamp: Stamp =
            serde_json::from_str(&std::fs::read_to_string(&stamp_path).unwrap()).unwrap();
        stamp.brain_uuid = uuid::Uuid::parse_str(&stamp.publication_uuid)
            .unwrap()
            .simple()
            .to_string()
            .to_uppercase();
        std::fs::write(&stamp_path, serde_json::to_string_pretty(&stamp).unwrap()).unwrap();
        std::fs::write(
            snap_dir.join(CHECKSUM_FILE),
            compute_checksums(&snap_dir).unwrap(),
        )
        .unwrap();

        let error = verify_snapshot(&snap_dir).unwrap_err().to_string();
        assert!(error.contains("must be distinct"), "{error}");
    }

    #[test]
    fn reader_remains_compatible_with_v2_snapshots_without_identity_fields() {
        let dir = tempfile::tempdir().unwrap();
        let snap_dir = dir.path().join("snapshot");
        let db = make_test_db(dir.path());
        build_snapshot(
            &snap_dir,
            &make_stamp("6.2.0", "4.1.1", "schema-hash-abc", "model"),
            &make_manifest(),
            &db,
        )
        .unwrap();

        // Model a snapshot emitted by the v2 writer: identity was neither a
        // declared capability nor part of the serialized stamp contract.
        let stamp_path = snap_dir.join(STAMP_FILE);
        let mut stamp: Stamp =
            serde_json::from_str(&std::fs::read_to_string(&stamp_path).unwrap()).unwrap();
        stamp.format_version = 2;
        stamp.capabilities = vec![SNAPSHOT_CAPABILITY_EMBEDDINGS.to_string()];
        stamp.brain_uuid.clear();
        stamp.publication_uuid.clear();
        stamp.min_compatible_engine = "4.1.1".to_string();
        std::fs::write(&stamp_path, serde_json::to_string_pretty(&stamp).unwrap()).unwrap();
        std::fs::write(
            snap_dir.join(CHECKSUM_FILE),
            compute_checksums(&snap_dir).unwrap(),
        )
        .unwrap();

        let verified = verify_snapshot(&snap_dir).unwrap();
        assert_eq!(verified.format_version, 2);
        assert!(verified.brain_uuid.is_empty());
        assert!(verified.publication_uuid.is_empty());
        load_snapshot_with_config(&snap_dir, "6.2.0", None, None)
            .expect("the v3 reader must retain v2 read compatibility");
    }

    #[test]
    fn load_rejects_incompatible_engine() {
        let dir = tempfile::tempdir().unwrap();
        let snap_dir = dir.path().join("snapshot");
        let db = make_test_db(dir.path());

        // Snapshot requires engine >= 2.0.0
        let stamp = make_stamp(
            "2.0.0",
            "2.0.0",
            "schema-hash-abc",
            "text-embedding-3-small",
        );
        let manifest = make_manifest();
        build_snapshot(&snap_dir, &stamp, &manifest, &db).unwrap();

        // Try to load with engine 1.0.0
        let result = load_snapshot(
            &snap_dir,
            "1.0.0",
            "schema-hash-abc",
            "text-embedding-3-small",
        );
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("engine") && (msg.contains("rebuild") || msg.contains("requires")),
            "unexpected error: {msg}"
        );
    }

    #[test]
    fn compatibility_gate_precedes_artifact_opening() {
        let dir = tempfile::tempdir().unwrap();
        let snap_dir = dir.path().join("snapshot");
        let db = make_test_db(dir.path());
        build_snapshot(
            &snap_dir,
            &make_stamp("99.0.0", "99.0.0", "schema-hash-abc", "model"),
            &make_manifest(),
            &db,
        )
        .unwrap();

        // Keep the envelope internally checksummed while making the graph
        // payload impossible to open. An incompatible reader must reject on
        // the declared engine contract before LadybugDB sees these bytes.
        std::fs::write(snap_dir.join(GRAPH_FILE), b"not a database").unwrap();
        std::fs::write(
            snap_dir.join(CHECKSUM_FILE),
            compute_checksums(&snap_dir).unwrap(),
        )
        .unwrap();

        let error = load_snapshot_with_config(&snap_dir, "6.3.0", None, None)
            .unwrap_err()
            .to_string();
        assert!(error.contains("requires engine >= 99.0.0"), "{error}");
        assert!(!error.contains("open snapshot graph"), "{error}");
    }

    #[test]
    fn load_rejects_mismatched_schema() {
        let dir = tempfile::tempdir().unwrap();
        let snap_dir = dir.path().join("snapshot");
        let db = make_test_db(dir.path());

        let stamp = make_stamp(
            "0.1.0",
            "0.1.0",
            "old-schema-hash",
            "text-embedding-3-small",
        );
        let manifest = make_manifest();
        build_snapshot(&snap_dir, &stamp, &manifest, &db).unwrap();

        let result = load_snapshot(
            &snap_dir,
            env!("CARGO_PKG_VERSION"),
            "new-schema-hash",
            "text-embedding-3-small",
        );
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("schema") && msg.contains("rebuild"),
            "unexpected error: {msg}"
        );
    }

    #[test]
    fn load_rejects_mismatched_embedding_model() {
        let dir = tempfile::tempdir().unwrap();
        let snap_dir = dir.path().join("snapshot");
        let db = make_test_db(dir.path());
        let store = nestweaver_store::GraphStore::open(&db).unwrap();
        store
            .set_embedding_metadata("text-embedding-3-small", 3)
            .unwrap();
        assert!(store.add_embedding("symbol:test", vec![1.0, 0.0, 0.0]));

        let stamp = make_stamp(
            "0.1.0",
            "0.1.0",
            "schema-hash-abc",
            "text-embedding-3-small",
        );
        let manifest = make_manifest();
        build_snapshot_from_store(&snap_dir, &stamp, &manifest, &store).unwrap();

        let result = load_snapshot(
            &snap_dir,
            env!("CARGO_PKG_VERSION"),
            "schema-hash-abc",
            "text-embedding-ada-002",
        );
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("embedding model") && msg.contains("rebuild"),
            "unexpected error: {msg}"
        );
    }

    #[test]
    fn verify_legacy_single_hash_checksum() {
        let dir = tempfile::tempdir().unwrap();
        let snap_dir = dir.path().join("snapshot");
        let db = make_test_db(dir.path());

        let stamp = make_stamp(
            "0.1.0",
            "0.1.0",
            "schema-hash-abc",
            "text-embedding-3-small",
        );
        let manifest = make_manifest();
        build_snapshot(&snap_dir, &stamp, &manifest, &db).unwrap();

        // Model an actual v2 snapshot before replacing its per-file checksum
        // with the legacy single-hash form. V3 must never accept this weaker
        // checksum because it would omit publication.json.
        let stamp_path = snap_dir.join(STAMP_FILE);
        let mut legacy_stamp: Stamp =
            serde_json::from_slice(&std::fs::read(&stamp_path).unwrap()).unwrap();
        legacy_stamp.format_version = 2;
        legacy_stamp.capabilities = vec![SNAPSHOT_CAPABILITY_EMBEDDINGS.to_string()];
        legacy_stamp.brain_uuid.clear();
        legacy_stamp.publication_uuid.clear();
        legacy_stamp.min_compatible_engine = "4.1.1".to_string();
        std::fs::write(
            &stamp_path,
            serde_json::to_vec_pretty(&legacy_stamp).unwrap(),
        )
        .unwrap();
        std::fs::remove_file(snap_dir.join(PUBLICATION_FILE)).unwrap();

        // BLAKE3 of graph.lbug + manifest.json + stamp.json concatenated.
        let mut hasher = blake3::Hasher::new();
        for name in CORE_FILES {
            hasher.update(&std::fs::read(snap_dir.join(name)).unwrap());
        }
        let legacy_checksum = hasher.finalize().to_hex().to_string();
        std::fs::write(snap_dir.join(CHECKSUM_FILE), &legacy_checksum).unwrap();

        let loaded = verify_snapshot(&snap_dir).unwrap();
        assert_eq!(loaded.instance_id, "test-instance");
    }

    #[test]
    fn snapshot_preserves_authoritative_embeddings_and_semantic_results() {
        let dir = tempfile::tempdir().unwrap();
        let snap_dir = dir.path().join("snapshot");
        let db = make_test_db(dir.path());
        let store = nestweaver_store::GraphStore::open(&db).unwrap();
        store.set_embedding_metadata("persisted-model", 3).unwrap();
        assert!(store.add_embedding("sym:a", vec![1.0, 0.0, 0.0]));
        assert!(store.add_embedding("sym:b", vec![0.0, 1.0, 0.0]));

        let stamp = build_snapshot_from_store(
            &snap_dir,
            &make_stamp("4.1.0", "0.11.0", "schema", "caller-lie"),
            &make_manifest(),
            &store,
        )
        .unwrap();
        assert_eq!(stamp.format_version, SNAPSHOT_FORMAT_VERSION);
        assert_eq!(stamp.embedding_model_id, "persisted-model");
        assert_eq!(stamp.embedding_dimension, 3);
        assert_eq!(stamp.embedding_count, 2);
        assert!(snap_dir.join(SIDECAR_EMBEDDINGS).exists());
        verify_snapshot(&snap_dir).unwrap();

        let work = dir.path().join("work");
        let materialized = materialize_snapshot_with_config(
            &snap_dir,
            &work,
            MIN_SNAPSHOT_READER_VERSION,
            None,
            None,
        )
        .unwrap();
        let replica = nestweaver_store::GraphStore::open_read_only(&materialized).unwrap();
        assert_eq!(replica.embedding_count(), 2);
        assert_eq!(replica.embedding_dimension().unwrap(), 3);
        assert_eq!(
            replica.try_vector_search(&[1.0, 0.0, 0.0], 1).unwrap()[0].0,
            "sym:a"
        );
    }

    #[test]
    fn v3_snapshot_fences_old_reader_but_current_reader_accepts_its_output() {
        let dir = tempfile::tempdir().unwrap();
        let snap_dir = dir.path().join("snapshot");
        let db = make_test_db(dir.path());

        let stamp = build_snapshot(
            &snap_dir,
            &make_stamp("4.1.0", "0.11.0", "schema", ""),
            &make_manifest(),
            &db,
        )
        .unwrap();

        assert_eq!(stamp.format_version, SNAPSHOT_FORMAT_VERSION);
        assert_eq!(stamp.min_compatible_engine, MIN_SNAPSHOT_READER_VERSION);
        assert!(
            !semver_ge(PRE_V3_READER, &stamp.min_compatible_engine),
            "the last pre-v3 reader must sit below the raised compatibility floor"
        );
        // Assert the fence actually FENCES. Previously this test only checked
        // the semver relation and then expected the same old reader to load,
        // which is self-contradictory the moment the floor rises past it.
        load_snapshot_with_config(&snap_dir, PRE_V3_READER, None, None)
            .expect_err("a pre-v3 reader must be refused by the raised floor");
        load_snapshot_with_config(&snap_dir, MIN_SNAPSHOT_READER_VERSION, None, None)
            .expect("the v3-capable reader must accept the v3 snapshot it wrote");
    }

    #[test]
    fn failed_restore_preserves_existing_replica() {
        let dir = tempfile::tempdir().unwrap();
        let snap_dir = dir.path().join("snapshot");
        let db = make_test_db(dir.path());
        let stamp = make_stamp("4.1.0", "0.11.0", "schema", "");
        build_snapshot(&snap_dir, &stamp, &make_manifest(), &db).unwrap();

        let work = dir.path().join("work");
        std::fs::create_dir(&work).unwrap();
        std::fs::write(work.join("sentinel"), b"old replica").unwrap();
        std::fs::write(snap_dir.join(GRAPH_FILE), b"tampered").unwrap();

        assert!(
            materialize_snapshot_with_config(
                &snap_dir,
                &work,
                MIN_SNAPSHOT_READER_VERSION,
                None,
                None
            )
            .is_err()
        );
        assert_eq!(
            std::fs::read(work.join("sentinel")).unwrap(),
            b"old replica"
        );
    }

    #[test]
    fn restore_reverifies_copied_bytes_before_atomic_promotion() {
        let dir = tempfile::tempdir().unwrap();
        let snap_dir = dir.path().join("snapshot");
        let db = make_test_db(dir.path());
        build_snapshot(
            &snap_dir,
            &make_stamp("4.1.0", "0.11.0", "schema", ""),
            &make_manifest(),
            &db,
        )
        .unwrap();

        let work = dir.path().join("work");
        std::fs::create_dir(&work).unwrap();
        std::fs::write(work.join("sentinel"), b"old replica").unwrap();

        let error = materialize_snapshot_with_config_and_hook(
            &snap_dir,
            &work,
            MIN_SNAPSHOT_READER_VERSION,
            None,
            None,
            || std::fs::write(snap_dir.join(GRAPH_FILE), b"changed after verification").unwrap(),
        )
        .unwrap_err();

        assert!(error.to_string().contains("integrity check failed"));
        assert_eq!(
            std::fs::read(work.join("sentinel")).unwrap(),
            b"old replica"
        );
        let staging_prefix = ".work.snapshot-restore.";
        assert!(
            std::fs::read_dir(dir.path()).unwrap().all(|entry| !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(staging_prefix)),
            "a rejected staged copy must be cleaned up"
        );
    }

    #[test]
    fn build_never_replaces_an_existing_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let snap_dir = dir.path().join("snapshot");
        std::fs::create_dir(&snap_dir).unwrap();
        std::fs::write(snap_dir.join("sentinel"), b"published snapshot").unwrap();
        let db = make_test_db(dir.path());

        let error = build_snapshot(
            &snap_dir,
            &make_stamp("4.1.0", "0.11.0", "schema", ""),
            &make_manifest(),
            &db,
        )
        .unwrap_err();
        assert!(error.to_string().contains("snapshots are immutable"));
        assert_eq!(
            std::fs::read(snap_dir.join("sentinel")).unwrap(),
            b"published snapshot"
        );
    }

    #[test]
    fn extension_snapshot_requires_config_but_core_snapshot_does_not() {
        let dir = tempfile::tempdir().unwrap();
        let core_dir = dir.path().join("core");
        let db = make_test_db(dir.path());
        let core = make_stamp("4.1.0", "0.11.0", "core-effective", "");
        build_snapshot(&core_dir, &core, &make_manifest(), &db).unwrap();
        load_snapshot_with_config(&core_dir, MIN_SNAPSHOT_READER_VERSION, None, None).unwrap();

        let extension_dir = dir.path().join("extension");
        let mut extension = core;
        extension.schema_hash_extensions = "extension-hash".to_string();
        extension.schema_hash_effective = "extension-effective".to_string();
        build_snapshot(&extension_dir, &extension, &make_manifest(), &db).unwrap();
        let error =
            load_snapshot_with_config(&extension_dir, MIN_SNAPSHOT_READER_VERSION, None, None)
                .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("requires the matching instance config")
        );
        load_snapshot_with_config(
            &extension_dir,
            MIN_SNAPSHOT_READER_VERSION,
            Some("extension-effective"),
            None,
        )
        .unwrap();
    }

    #[test]
    fn legacy_claimed_embeddings_without_artifact_fail_closed() {
        let dir = tempfile::tempdir().unwrap();
        let snapshot = dir.path();
        let mut stamp = make_stamp("0.10.0", "0.10.0", "schema", "legacy-model");
        stamp.format_version = 0;
        stamp.embedding_dimension = 3;
        std::fs::write(snapshot.join(GRAPH_FILE), b"graph").unwrap();
        std::fs::write(snapshot.join(MANIFEST_FILE), b"{\"repos\":[]}").unwrap();
        std::fs::write(
            snapshot.join(STAMP_FILE),
            serde_json::to_vec_pretty(&stamp).unwrap(),
        )
        .unwrap();
        let mut hasher = blake3::Hasher::new();
        for name in CORE_FILES {
            hasher.update(&std::fs::read(snapshot.join(name)).unwrap());
        }
        std::fs::write(
            snapshot.join(CHECKSUM_FILE),
            hasher.finalize().to_hex().as_bytes(),
        )
        .unwrap();
        let error = verify_snapshot(snapshot).unwrap_err();
        assert!(error.to_string().contains("omits embeddings.bin"));
    }
}
