use serde::{Deserialize, Serialize};
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// The oldest engine version that can read the current snapshot format.
///
/// Bump this ONLY when the snapshot layout changes in a backwards-incompatible
/// way (new required files, changed checksum format, etc.).  Routine engine
/// releases that don't touch the snapshot wire format should leave this alone.
pub const MIN_SNAPSHOT_READER_VERSION: &str = "0.11.0";
pub const SNAPSHOT_FORMAT_VERSION: u32 = 2;
pub const SNAPSHOT_CAPABILITY_EMBEDDINGS: &str = "embedding-sidecar-v1";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Stamp {
    #[serde(default)]
    pub format_version: u32,
    #[serde(default)]
    pub capabilities: Vec<String>,
    pub instance_id: String,
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
/// Sidecar filenames (relative to the db_path prefix, not snapshot_dir).
const SIDECAR_PAGERANK: &str = "pagerank.json";
const SIDECAR_MANIFESTS: &str = "manifests.json";
const SIDECAR_TANTIVY_DIR: &str = "tantivy";
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
        let bytes = std::fs::read(snapshot_dir.join(name))
            .map_err(|e| anyhow::anyhow!("failed to read {name} for checksum: {e}"))?;
        let hash = crate::hash::blake3_hex_bytes(&bytes);
        lines.push(format!("{hash}  {name}"));
    }
    for &name in SIDECAR_FILES {
        let path = snapshot_dir.join(name);
        if path.exists() {
            let bytes = std::fs::read(&path)
                .map_err(|e| anyhow::anyhow!("failed to read sidecar {name} for checksum: {e}"))?;
            let hash = crate::hash::blake3_hex_bytes(&bytes);
            lines.push(format!("{hash}  {name}"));
        }
    }
    // Hash tantivy directory contents if present (hash each file, sorted)
    let tantivy_dir = snapshot_dir.join(SIDECAR_TANTIVY_DIR);
    if tantivy_dir.is_dir() {
        let mut tantivy_files = collect_files_recursive(&tantivy_dir, SIDECAR_TANTIVY_DIR)?;
        tantivy_files.sort();
        for (rel_path, abs_path) in tantivy_files {
            let bytes = std::fs::read(&abs_path)
                .map_err(|e| anyhow::anyhow!("failed to read {rel_path} for checksum: {e}"))?;
            let hash = crate::hash::blake3_hex_bytes(&bytes);
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
            let bytes = std::fs::read(&file_path)
                .map_err(|e| anyhow::anyhow!("failed to read {filename}: {e}"))?;
            let actual = crate::hash::blake3_hex_bytes(&bytes);
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
            let bytes = std::fs::read(snapshot_dir.join(name))
                .map_err(|e| anyhow::anyhow!("failed to read {name} for checksum: {e}"))?;
            hasher.update(&bytes);
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
        authoritative_stamp.format_version = SNAPSHOT_FORMAT_VERSION;
        authoritative_stamp.capabilities = vec![SNAPSHOT_CAPABILITY_EMBEDDINGS.to_string()];
        authoritative_stamp.embedding_model_id = embedding.model_id.clone();
        authoritative_stamp.embedding_dimension = embedding.dimension;
        authoritative_stamp.embedding_count = embedding.count;
        build_snapshot_files(&staging, &authoritative_stamp, manifest, db_path)?;
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

fn build_snapshot_files(
    output_dir: &Path,
    stamp: &Stamp,
    manifest: &Manifest,
    db_path: &Path,
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
        std::fs::copy(&pagerank_src, output_dir.join(SIDECAR_PAGERANK)).map_err(|e| {
            anyhow::anyhow!(
                "failed to copy pagerank sidecar {}: {e}",
                pagerank_src.display()
            )
        })?;
    } else {
        tracing::debug!(
            src = %pagerank_src.display(),
            "build_snapshot: pagerank sidecar not found, skipping"
        );
    }

    let manifests_src = crate::sidecar_path(db_path, &format!(".{SIDECAR_MANIFESTS}"));
    if manifests_src.exists() {
        std::fs::copy(&manifests_src, output_dir.join(SIDECAR_MANIFESTS)).map_err(|e| {
            anyhow::anyhow!(
                "failed to copy manifests sidecar {}: {e}",
                manifests_src.display()
            )
        })?;
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

    // ── Checksums (after all files are in place) ────────────────────────────
    let checksums = compute_checksums(output_dir)?;
    std::fs::write(output_dir.join(CHECKSUM_FILE), &checksums)?;

    Ok(())
}

/// Verify a snapshot directory's integrity.
///
/// Supports both the new per-file checksum format and the legacy single-hash
/// format for backwards compatibility.
pub fn verify_snapshot(snapshot_dir: &Path) -> Result<Stamp, anyhow::Error> {
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
    if embedding_path.exists() {
        let embeddings = nestweaver_store::EmbeddingIndex::load_binary(&embedding_path)
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
/// 1. `verify_snapshot()` — integrity check.
/// 2. Check `stamp.min_compatible_engine <= engine_version` (stamp requires at least that version).
/// 3. Check `stamp.schema_hash_effective == expected_schema_hash`.
/// 4. Check `stamp.embedding_model_id == expected_embedding_model`.
/// 5. Return `(stamp, path to graph.lbug)`.
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
    let stamp = verify_snapshot(snapshot_dir)?;

    // The snapshot requires at least min_compatible_engine to load it.
    // If the running engine_version < min_compatible_engine, reject.
    if !semver_ge(engine_version, &stamp.min_compatible_engine) {
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
    // Compat gate first — refuse an incompatible snapshot before touching disk.
    load_snapshot_with_config(
        snapshot_dir,
        engine_version,
        expected_schema_hash,
        expected_embedding_model,
    )?;

    let staging = sibling_staging_path(working_dir, "snapshot-restore")?;
    if staging.exists() {
        std::fs::remove_dir_all(&staging)?;
    }
    std::fs::create_dir(&staging)?;
    let db_path = staging.join(GRAPH_FILE);
    std::fs::copy(snapshot_dir.join(GRAPH_FILE), &db_path)
        .map_err(|e| anyhow::anyhow!("failed to copy snapshot graph file: {e}"))?;

    // Relocate JSON sidecars into the store's `<db><suffix>` convention.
    for (src_name, suffix) in [
        (SIDECAR_PAGERANK, ".pagerank.json"),
        (SIDECAR_MANIFESTS, ".manifests.json"),
        (SIDECAR_EMBEDDINGS, ".embeddings.bin"),
    ] {
        let src = snapshot_dir.join(src_name);
        if src.exists() {
            let dst = crate::sidecar_path(&db_path, suffix);
            std::fs::copy(&src, &dst).map_err(|e| {
                anyhow::anyhow!("failed to restore snapshot sidecar {}: {e}", src.display())
            })?;
        }
    }

    // Relocate the Tantivy index directory into `<db>.tantivy`.
    let tantivy_src = snapshot_dir.join(SIDECAR_TANTIVY_DIR);
    if tantivy_src.is_dir() {
        let tantivy_dst = crate::sidecar_path(&db_path, ".tantivy");
        nestweaver_storage::copy_dir_all(&tantivy_src, &tantivy_dst)
            .map_err(|e| anyhow::anyhow!("failed to restore snapshot tantivy index: {e}"))?;
    }
    sync_directory_tree(&staging)?;
    publish_restored_directory(&staging, working_dir)?;
    Ok(working_dir.join(GRAPH_FILE))
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

        build_snapshot(&snap_dir, &stamp, &manifest, &db).unwrap();

        assert!(snap_dir.join(GRAPH_FILE).exists(), "graph.lbug missing");
        assert!(
            snap_dir.join(MANIFEST_FILE).exists(),
            "manifest.json missing"
        );
        assert!(snap_dir.join(STAMP_FILE).exists(), "stamp.json missing");
        assert!(
            snap_dir.join(CHECKSUM_FILE).exists(),
            "checksum.blake3 missing"
        );
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
        std::fs::write(crate::sidecar_path(&db, ".pagerank.json"), b"{}").unwrap();
        std::fs::write(crate::sidecar_path(&db, ".manifests.json"), b"{}").unwrap();

        let stamp = make_stamp(
            "0.1.0",
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
            "0.1.0",
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
                "0.1.0",
                "WRONG-schema-hash",
                "text-embedding-3-small",
            )
            .is_err(),
            "an incompatible snapshot must refuse to materialize"
        );
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
            "0.1.0",
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

        let stamp = make_stamp(
            "0.1.0",
            "0.1.0",
            "schema-hash-abc",
            "text-embedding-3-small",
        );
        let manifest = make_manifest();
        build_snapshot(&snap_dir, &stamp, &manifest, &db).unwrap();

        let result = load_snapshot(
            &snap_dir,
            "0.1.0",
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

        // Replace the per-file checksum with a legacy single-hash checksum
        // (BLAKE3 of graph.lbug + manifest.json + stamp.json concatenated).
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
        let materialized =
            materialize_snapshot_with_config(&snap_dir, &work, "4.1.0", None, None).unwrap();
        let replica = nestweaver_store::GraphStore::open_read_only(&materialized).unwrap();
        assert_eq!(replica.embedding_count(), 2);
        assert_eq!(replica.embedding_dimension().unwrap(), 3);
        assert_eq!(replica.vector_search(&[1.0, 0.0, 0.0], 1)[0].0, "sym:a");
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

        assert!(materialize_snapshot_with_config(&snap_dir, &work, "4.1.0", None, None).is_err());
        assert_eq!(
            std::fs::read(work.join("sentinel")).unwrap(),
            b"old replica"
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
        load_snapshot_with_config(&core_dir, "4.1.0", None, None).unwrap();

        let extension_dir = dir.path().join("extension");
        let mut extension = core;
        extension.schema_hash_extensions = "extension-hash".to_string();
        extension.schema_hash_effective = "extension-effective".to_string();
        build_snapshot(&extension_dir, &extension, &make_manifest(), &db).unwrap();
        let error = load_snapshot_with_config(&extension_dir, "4.1.0", None, None).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("requires the matching instance config")
        );
        load_snapshot_with_config(&extension_dir, "4.1.0", Some("extension-effective"), None)
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
