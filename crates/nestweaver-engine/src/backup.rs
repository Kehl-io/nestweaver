use anyhow::Context;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Known sidecar suffixes to include in backups.
const SIDECAR_SUFFIXES: &[&str] = &[
    ".tantivy",
    ".regex-v3",
    ".pagerank.json",
    ".parsed_cache.bin",
    ".resolution_deps.bin",
    crate::resolver_generation::RESOLVER_GENERATION_SIDECAR,
    ".filemeta.json",
    ".manifests.json",
    ".gitactivity.json",
    ".cochange.json",
    ".interactions.json",
    ".extensions.json",
    ".extensions.migration.json",
    ".extensions.handoff.json",
    ".aliases.json",
    ".bundles.json",
    ".generation",
    ".embeddings.bin",
    ".embeddings",
    crate::publication::SOURCE_MANIFEST_SUFFIX,
    crate::publication::PRESERVED_STATE_SUFFIX,
];

/// Current backup manifest version. Version 2 embeds the same typed
/// `PublicationBundleV3` inventory used by snapshots and publication cutover.
const MANIFEST_VERSION: u32 = 2;

/// Hard ceiling for the in-archive `publication.json`, which is the only member
/// read into memory rather than streamed. It is a JSON inventory of artifact
/// descriptors — a few hundred KB even for a large publication — so 64 MiB is
/// generous while still bounding a hostile or corrupt header (nw-144).
const MAX_PUBLICATION_MANIFEST_BYTES: u64 = 64 * 1024 * 1024;

#[derive(Debug, Clone)]
pub struct BackupConfig {
    pub db_path: PathBuf,
    pub output_path: PathBuf,
    pub include_clones: bool,
    pub instance_id: String,
    pub workspace_path: Option<PathBuf>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackupManifest {
    pub version: u32,
    pub tier: String,
    pub nestweaver_version: String,
    pub schema_version: u32,
    pub created_at: String,
    pub instance_id: String,
    #[serde(default)]
    pub brain_uuid: String,
    #[serde(default)]
    pub publication_uuid: String,
    #[serde(default)]
    pub publication_manifest_blake3: String,
    pub repos: Vec<BackupRepoInfo>,
    pub repo_count: usize,
    pub symbol_count: usize,
    pub sizes: BackupSizes,
    pub checksums: HashMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackupRepoInfo {
    pub url: String,
    pub indexed_sha: String,
    pub symbols: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackupSizes {
    pub db: u64,
    pub tantivy: u64,
    pub parsed_cache: u64,
    pub total_uncompressed: u64,
    pub total_compressed: u64,
}

#[derive(Debug)]
pub struct BackupResult {
    pub manifest: BackupManifest,
    pub output_path: PathBuf,
    pub duration: Duration,
    pub write_pause_duration: Duration,
}

#[derive(Debug, Clone)]
pub struct RestoreConfig {
    pub snapshot_path: PathBuf,
    pub data_dir: PathBuf,
}

#[derive(Debug)]
pub struct RestoreResult {
    pub manifest: BackupManifest,
    pub data_dir: PathBuf,
    pub duration: Duration,
}

/// Seal an already-built publication slot in its live store layout.
///
/// Unlike an archive backup this performs no copying: the caller builds the
/// graph and sidecars directly beneath `slot_root`, then this function derives
/// one canonical, identity-bound inventory and durably publishes
/// `publication.json` last. A half-built slot is therefore never selectable.
pub fn seal_publication_slot(
    db_path: &Path,
    slot_root: &Path,
) -> anyhow::Result<crate::publication::PublicationBundleV3> {
    let db_parent = db_path
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    if std::fs::canonicalize(db_parent)? != std::fs::canonicalize(slot_root)? {
        anyhow::bail!(
            "publication database {} must live directly beneath slot root {}",
            db_path.display(),
            slot_root.display()
        );
    }
    let store = nestweaver_store::GraphStore::open_read_only(db_path)
        .map_err(|error| anyhow::anyhow!("open staged publication graph: {error}"))?;
    let identity = store
        .publication_identity()
        .map_err(|error| anyhow::anyhow!("read staged publication identity: {error}"))?
        .ok_or_else(|| anyhow::anyhow!("staged publication graph has no identity"))?;
    let source_graph_generation = store.graph_generation();
    drop(store);

    let config = BackupConfig {
        db_path: db_path.to_path_buf(),
        output_path: slot_root.join("unused.nwsnap.zst"),
        include_clones: false,
        instance_id: "publication".to_string(),
        workspace_path: None,
    };
    let bundle =
        build_backup_publication_bundle(&config, slot_root, &identity, source_graph_generation)?;
    let bytes = serde_json::to_vec_pretty(&bundle)?;
    let manifest_path = slot_root.join(crate::publication::PUBLICATION_MANIFEST_FILE);
    nestweaver_store::durable_sidecar::atomic_replace_file(&manifest_path, |file| {
        use std::io::Write as _;
        file.write_all(&bytes)?;
        file.write_all(b"\n")
    })?;
    Ok(bundle)
}

/// Create a backup of the NestWeaver database and all sidecar files.
///
/// A backup staged (files copied, stats gathered) while the publication lease
/// was held, ready to package lock-free via [`package_staged`].
pub struct StagedBackup {
    staging: tempfile::TempDir,
    repos: Vec<BackupRepoInfo>,
    symbol_count: usize,
    start: Instant,
    write_pause: Duration,
    publication_identity: nestweaver_store::PublicationIdentity,
    source_graph_generation: u64,
}

/// Stage a backup from an ALREADY-OPEN store: flush embeddings, `CHECKPOINT` the
/// WAL into the main file, copy the on-disk files to a temp staging dir, and
/// gather manifest stats. Reuses the caller's live connection — it never opens a
/// second one. It also owns the store's publication lease through checkpoint,
/// copy, and statistics collection, so UI watchers and other publishers cannot
/// mutate the graph mid-copy. The configured path must resolve to this store;
/// all database and sidecar reads derive from `store.db_path()`. An inherited
/// dirty publication is rejected.
/// The returned [`StagedBackup`] is packaged lock-free by [`package_staged`].
pub fn stage_backup_from_store(
    store: &nestweaver_store::GraphStore,
    config: &BackupConfig,
) -> anyhow::Result<StagedBackup> {
    let store_db_path = store
        .db_path()
        .ok_or_else(|| anyhow::anyhow!("cannot back up an in-memory graph store"))?;
    let configured_db = std::fs::canonicalize(&config.db_path).with_context(|| {
        format!(
            "failed to resolve configured backup database {}",
            config.db_path.display()
        )
    })?;
    let opened_db = std::fs::canonicalize(store_db_path).with_context(|| {
        format!(
            "failed to resolve opened backup database {}",
            store_db_path.display()
        )
    })?;
    if configured_db != opened_db {
        anyhow::bail!(
            "configured backup database {} does not match opened store {}",
            config.db_path.display(),
            store_db_path.display()
        );
    }
    let staged_db_filename = config
        .db_path
        .file_name()
        .ok_or_else(|| anyhow::anyhow!("db_path has no filename"))?;

    let start = Instant::now();
    let pause_start = Instant::now();
    let publication = store
        .acquire_index_publication_lease()
        .map_err(|error| anyhow::anyhow!("failed to quiesce graph publication: {error}"))?;
    publication
        .ensure_clean_for_snapshot()
        .map_err(|error| anyhow::anyhow!("refusing backup of dirty index publication: {error}"))?;
    let staging = tempfile::tempdir()?;

    store
        .compact_embedding_index()
        .map_err(|e| anyhow::anyhow!("failed to compact embedding index: {e}"))?;
    store
        .checkpoint()
        .map_err(|e| anyhow::anyhow!("CHECKPOINT failed: {e}"))?;

    // Copy files while the caller holds the write lock (sidecars are non-atomic).
    copy_db_files(
        store_db_path,
        staged_db_filename,
        staging.path(),
        config.include_clones,
        config.workspace_path.as_deref(),
    )?;

    // Gather graph statistics for the manifest while the store is open.
    let symbol_count = store.count_symbols().unwrap_or(0);
    let per_repo = store.count_symbols_by_repo().unwrap_or_default();
    let repos: Vec<BackupRepoInfo> = store
        .list_repos(None)
        .unwrap_or_default()
        .into_iter()
        .map(|r| BackupRepoInfo {
            symbols: per_repo.get(&r.uid).copied().unwrap_or(0),
            url: r.url,
            indexed_sha: r.indexed_sha,
        })
        .collect();
    let publication_identity = store
        .publication_identity()
        .map_err(|error| anyhow::anyhow!("read backup publication identity: {error}"))?
        .ok_or_else(|| anyhow::anyhow!("backup graph has no publication identity"))?;
    let source_graph_generation = store.graph_generation();

    let write_pause = pause_start.elapsed();
    publication
        .release()
        .map_err(|error| anyhow::anyhow!("failed to release backup publication lease: {error}"))?;
    Ok(StagedBackup {
        staging,
        repos,
        symbol_count,
        start,
        write_pause,
        publication_identity,
        source_graph_generation,
    })
}

pub fn backup_save(config: &BackupConfig) -> anyhow::Result<BackupResult> {
    let store = nestweaver_store::GraphStore::open(&config.db_path)
        .map_err(|e| anyhow::anyhow!("failed to open database: {e}"))?;
    let staged = stage_backup_from_store(&store, config)?;
    drop(store);
    package_staged(config, staged)
}

/// Package a [`StagedBackup`] into the `.nwsnap.zst` archive. Runs lock-free,
/// after the write lock has been released.
pub fn package_staged(config: &BackupConfig, staged: StagedBackup) -> anyhow::Result<BackupResult> {
    let StagedBackup {
        staging,
        repos,
        symbol_count,
        start,
        write_pause,
        publication_identity,
        source_graph_generation,
    } = staged;
    let bundle = build_backup_publication_bundle(
        config,
        staging.path(),
        &publication_identity,
        source_graph_generation,
    )?;
    let publication_bytes = serde_json::to_vec_pretty(&bundle)?;
    std::fs::write(
        staging
            .path()
            .join(crate::publication::PUBLICATION_MANIFEST_FILE),
        &publication_bytes,
    )?;
    let publication_manifest_blake3 = crate::hash::blake3_hex_bytes(&publication_bytes);
    let mut manifest = build_backup_manifest(
        config,
        staging.path(),
        repos,
        symbol_count,
        &bundle,
        publication_manifest_blake3,
    )?;
    let manifest_json = serde_json::to_string_pretty(&manifest)?;
    std::fs::write(staging.path().join("manifest.json"), &manifest_json)?;

    package_tar_zstd(staging.path(), &config.output_path)?;

    // The compressed size is only known after packaging, so it cannot live in
    // the sealed in-archive manifest. Fill it on the returned manifest here;
    // backup_inspect recomputes it from the archive on disk.
    manifest.sizes.total_compressed = std::fs::metadata(&config.output_path)
        .map(|m| m.len())
        .unwrap_or(0);

    Ok(BackupResult {
        manifest,
        output_path: config.output_path.clone(),
        duration: start.elapsed(),
        write_pause_duration: write_pause,
    })
}

/// Return whether an archive member is a regular payload file.
///
/// NestWeaver emits only directories and regular files. Links and special
/// entries are neither part of the canonical publication inventory nor safe
/// to materialize during restore.
fn backup_archive_member_is_payload(
    path: &str,
    entry_type: tar::EntryType,
) -> anyhow::Result<bool> {
    if entry_type.is_dir() {
        return Ok(false);
    }
    if entry_type.is_file() {
        return Ok(true);
    }
    anyhow::bail!("unsupported backup archive member type for {path}: {entry_type:?}")
}

/// Read the manifest from an existing `.nwsnap.zst` archive without full extraction.
///
/// Verifies that file sizes in the archive match the manifest checksums entries
/// and recomputes checksums for integrity verification.
pub fn backup_inspect(archive_path: &Path) -> anyhow::Result<BackupManifest> {
    let file = std::fs::File::open(archive_path)?;
    let decoder = nestweaver_store::zstd::Decoder::new(file)?;
    let mut archive = tar::Archive::new(decoder);

    let mut manifest: Option<BackupManifest> = None;
    let mut publication: Option<crate::publication::PublicationBundleV3> = None;
    let mut archive_file_sizes: HashMap<String, u64> = HashMap::new();
    let mut archive_checksums: HashMap<String, String> = HashMap::new();
    let mut archive_blake3: HashMap<String, String> = HashMap::new();

    for entry in archive.entries()? {
        let mut entry = entry?;
        let path = entry.path()?.to_string_lossy().to_string();
        let normalized = path.strip_prefix("./").unwrap_or(&path).to_string();

        // Tar directory members are not required to end in `/`. Classifying
        // them from the rendered path therefore turns every nested sidecar
        // directory into an apparent, unmanifested payload. Trust the header
        // type instead, and fail closed on links/devices that NestWeaver never
        // emits and must not materialize during restore.
        if !backup_archive_member_is_payload(&normalized, entry.header().entry_type())? {
            continue;
        }

        if normalized == "manifest.json" {
            let m: BackupManifest = serde_json::from_reader(&mut entry)?;
            manifest = Some(m);
        } else if normalized == crate::publication::PUBLICATION_MANIFEST_FILE {
            // nw-144: `size` is the untrusted tar header field. Pre-allocating
            // from it turns a crafted or bit-rotted 8-byte header into an
            // unbounded allocation, and Rust's alloc handler ABORTS the process
            // (uncatchable) — a 48 KB archive was enough to kill `backup
            // inspect` and `backup list` (CWE-789). publication.json is a small
            // JSON manifest, so read it through a hard cap instead; every
            // sibling member below is already streamed for the same reason.
            let size = entry.header().size()?;
            anyhow::ensure!(
                size <= MAX_PUBLICATION_MANIFEST_BYTES,
                "publication.json declares {size} bytes, above the {MAX_PUBLICATION_MANIFEST_BYTES}-byte ceiling"
            );
            let mut bytes = Vec::new();
            std::io::Read::read_to_end(
                &mut std::io::Read::take(&mut entry, MAX_PUBLICATION_MANIFEST_BYTES),
                &mut bytes,
            )?;
            anyhow::ensure!(
                bytes.len() as u64 == size,
                "publication.json is {} bytes but its header declares {size}",
                bytes.len()
            );
            archive_file_sizes.insert(normalized.clone(), size);
            let (sha256, blake3) = hash_stream_both(std::io::Cursor::new(&bytes))?;
            archive_checksums.insert(normalized.clone(), sha256);
            archive_blake3.insert(normalized, blake3);
            publication = Some(serde_json::from_slice(&bytes)?);
        } else if !normalized.is_empty() {
            // Track file sizes for verification.
            let size = entry.header().size()?;
            archive_file_sizes.insert(normalized.clone(), size);

            // Compute checksum for files listed in manifest checksums (streamed,
            // so a multi-GB member never lands in memory).
            let (sha256, blake3) = hash_stream_both(&mut entry)?;
            archive_checksums.insert(normalized.clone(), sha256);
            archive_blake3.insert(normalized, blake3);
        }
    }

    let mut manifest =
        manifest.ok_or_else(|| anyhow::anyhow!("manifest.json not found in archive"))?;

    if manifest.version >= 2 {
        let publication = publication
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("publication.json not found in v2 backup archive"))?;
        validate_backup_publication_inventory(
            &manifest,
            publication,
            &archive_file_sizes,
            &archive_blake3,
        )?;
    }

    // The in-archive manifest cannot record its own compressed size
    // (sealed before compression finishes). Recompute it from the
    // archive file on disk so inspect/list report a real figure.
    if manifest.sizes.total_compressed == 0 {
        manifest.sizes.total_compressed = std::fs::metadata(archive_path)
            .map(|m| m.len())
            .unwrap_or(0);
    }

    // Verify checksums: each file referenced in the manifest must have a
    // matching checksum in the archive.
    for (filename, expected_hash) in &manifest.checksums {
        match archive_checksums.get(filename) {
            Some(actual_hash) if actual_hash == expected_hash => {}
            Some(actual_hash) => {
                anyhow::bail!(
                    "integrity check failed for {filename}: expected {expected_hash}, got {actual_hash}"
                );
            }
            None => {
                anyhow::bail!("checksum references file not found in archive: {filename}");
            }
        }
    }

    Ok(manifest)
}

/// List all `.nwsnap.zst` backups in a directory, sorted by creation time.
pub fn backup_list(dir: &Path) -> anyhow::Result<Vec<(PathBuf, BackupManifest)>> {
    let mut results = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) == Some("zst")
            && path.to_string_lossy().contains(".nwsnap.")
        {
            match backup_inspect(&path) {
                Ok(manifest) => results.push((path, manifest)),
                Err(e) => {
                    tracing::warn!("skipping {}: {e}", path.display());
                }
            }
        }
    }
    results.sort_by(|a, b| a.1.created_at.cmp(&b.1.created_at));
    Ok(results)
}

/// Reconcile a leftover `<data>.restoring` directory from a previously
/// interrupted restore, before starting a new one.
///
/// The restore uses a rename-aside dance: move `data_dir` -> `.restoring`, then
/// move the new data into `data_dir`, then delete `.restoring`. If a prior
/// restore crashed *between* those first two steps, `data_dir` is gone and
/// `.restoring` holds the ONLY surviving copy of the old data. In that case we
/// rename it back into place rather than destroying it. If `data_dir` is present,
/// `.restoring` is a harmless orphan (crash after the new data landed) and is
/// removed.
fn recover_interrupted_restore(data_dir: &Path) -> anyhow::Result<()> {
    let restoring_dir = data_dir.with_extension("restoring");
    if !restoring_dir.exists() {
        return Ok(());
    }
    if dir_is_present(data_dir) {
        // Data dir intact — `.restoring` is a harmless orphan.
        let _ = std::fs::remove_dir_all(&restoring_dir);
    } else {
        // Data dir missing/empty — `.restoring` is the only surviving copy.
        std::fs::rename(&restoring_dir, data_dir).with_context(|| {
            format!(
                "recover interrupted restore: rename {} back to {}",
                restoring_dir.display(),
                data_dir.display()
            )
        })?;
    }
    Ok(())
}

/// True if `dir` exists and contains at least one entry. A missing or empty
/// data dir means a prior restore had already moved the real data aside.
fn dir_is_present(dir: &Path) -> bool {
    std::fs::read_dir(dir)
        .map(|mut it| it.next().is_some())
        .unwrap_or(false)
}

/// Restore a backup archive into a target directory.
///
/// Extracts to a temporary directory first, verifies integrity, then
/// atomically renames to the target. If verification fails, the temp
/// directory is cleaned up and the target is left untouched.
pub fn backup_restore(config: &RestoreConfig) -> anyhow::Result<RestoreResult> {
    let start = Instant::now();

    // Extract to a sibling temp directory so we can atomically rename on success.
    let parent = config
        .data_dir
        .parent()
        .unwrap_or(std::path::Path::new("."));
    std::fs::create_dir_all(parent)?;
    let temp_dir = tempfile::tempdir_in(parent)?;

    let file = std::fs::File::open(&config.snapshot_path)?;
    let decoder = nestweaver_store::zstd::Decoder::new(file)?;
    let mut archive = tar::Archive::new(decoder);
    for entry in archive.entries()? {
        let mut entry = entry?;
        let path = entry.path()?.to_string_lossy().to_string();
        let normalized = path.strip_prefix("./").unwrap_or(&path).to_string();
        let _ = backup_archive_member_is_payload(&normalized, entry.header().entry_type())?;
        if !entry.unpack_in(temp_dir.path())? {
            anyhow::bail!("unsafe backup archive member path: {normalized}");
        }
    }

    let manifest_path = temp_dir.path().join("manifest.json");
    let manifest_str = std::fs::read_to_string(&manifest_path)
        .map_err(|e| anyhow::anyhow!("failed to read manifest.json after extraction: {e}"))?;
    let manifest: BackupManifest = serde_json::from_str(&manifest_str)?;

    // Verify integrity before committing to the target directory.
    if let Err(e) = verify_backup_checksums(temp_dir.path(), &manifest) {
        // Clean up the temp directory (happens automatically on drop, but
        // be explicit for clarity).
        drop(temp_dir);
        return Err(anyhow::anyhow!(
            "restore aborted — integrity check failed: {e}"
        ));
    }
    check_schema_compatibility(&manifest)?;
    if manifest.version >= 2 {
        let publication_path = temp_dir
            .path()
            .join(crate::publication::PUBLICATION_MANIFEST_FILE);
        let publication_bytes = std::fs::read(&publication_path)?;
        let publication: crate::publication::PublicationBundleV3 =
            serde_json::from_slice(&publication_bytes)?;
        let mut sizes = HashMap::new();
        let mut hashes = HashMap::new();
        for entry in walkdir::WalkDir::new(temp_dir.path())
            .into_iter()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_type().is_file())
        {
            let path = normalized_relative_path(temp_dir.path(), entry.path())?;
            let (byte_size, digest) = crate::hash::blake3_file(entry.path())?;
            sizes.insert(path.clone(), byte_size);
            hashes.insert(path, digest);
        }
        validate_backup_publication_inventory(&manifest, &publication, &sizes, &hashes)?;

        let graph = publication
            .artifacts
            .iter()
            .find(|artifact| artifact.kind == crate::publication::ArtifactKind::Graph)
            .ok_or_else(|| anyhow::anyhow!("backup publication has no graph artifact"))?;
        let graph_store =
            nestweaver_store::GraphStore::open_read_only(&temp_dir.path().join(&graph.path))
                .map_err(|error| anyhow::anyhow!("open restored graph identity: {error}"))?;
        let graph_identity = graph_store
            .publication_identity()
            .map_err(|error| anyhow::anyhow!("read restored graph identity: {error}"))?
            .ok_or_else(|| anyhow::anyhow!("restored graph has no publication identity"))?;
        if graph_identity.brain_uuid != publication.brain_uuid
            || graph_identity.publication_uuid != publication.publication_uuid
        {
            anyhow::bail!(
                "backup publication identity mismatch: manifest {}/{}, graph {}/{}",
                publication.brain_uuid,
                publication.publication_uuid,
                graph_identity.brain_uuid,
                graph_identity.publication_uuid
            );
        }
    }

    // The backup saves clones under `clones/` (historical name), but the
    // daemon and CLI expect them at `workspace/`. Rename after extraction
    // so the restored layout matches runtime expectations.
    let extracted_clones = temp_dir.path().join("clones");
    let extracted_workspace = temp_dir.path().join("workspace");
    if extracted_clones.exists() && !extracted_workspace.exists() {
        std::fs::rename(&extracted_clones, &extracted_workspace).with_context(|| {
            format!(
                "rename clones/ to workspace/ in extracted backup at {}",
                temp_dir.path().display()
            )
        })?;
    }

    // Move the verified extraction to the target directory. If the target
    // already exists, remove it first (we've already verified the new data).
    // Atomic restore: rename-aside pattern (similar to dpkg atomic upgrades).
    let restoring_dir = config.data_dir.with_extension("restoring");

    // Reconcile any leftover .restoring dir from a previously interrupted
    // restore. If it holds the only surviving copy (data dir gone), recover it
    // instead of deleting it.
    recover_interrupted_restore(&config.data_dir)?;

    if config.data_dir.exists() {
        // Step 1: Move existing data aside (crash here = old data at .restoring, recoverable).
        std::fs::rename(&config.data_dir, &restoring_dir).with_context(|| {
            format!(
                "rename {} to {}",
                config.data_dir.display(),
                restoring_dir.display()
            )
        })?;
    }

    // Step 2: Move new data into place (crash here = old data at .restoring, recoverable).
    if std::fs::rename(temp_dir.path(), &config.data_dir).is_err() {
        // Cross-device: fall back to copy.
        if let Err(e) = nestweaver_storage::copy_dir_all(temp_dir.path(), &config.data_dir) {
            // Partial write may have occurred. Log recovery instructions before
            // propagating so the user knows the old data is still available.
            tracing::error!(
                "Cross-device copy failed: {e}. Your previous data is preserved at '{}'. \
                 To recover, remove the partially-written '{}' and rename '{}' back to '{}'.",
                restoring_dir.display(),
                config.data_dir.display(),
                restoring_dir.display(),
                config.data_dir.display(),
            );
            return Err(e).with_context(|| {
                format!(
                    "cross-device restore copy failed; old data recoverable from {}",
                    restoring_dir.display()
                )
            });
        }
    }

    // Step 3: Remove the old data (crash here = orphan .restoring, harmless).
    if restoring_dir.exists() {
        let _ = std::fs::remove_dir_all(&restoring_dir);
    }

    Ok(RestoreResult {
        manifest,
        data_dir: config.data_dir.clone(),
        duration: start.elapsed(),
    })
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// Copy the database file and all existing sidecars into the staging directory.
fn copy_db_files(
    db_path: &Path,
    staged_db_filename: &std::ffi::OsStr,
    staging: &Path,
    include_clones: bool,
    workspace_path: Option<&Path>,
) -> anyhow::Result<()> {
    // Copy the main database file.
    std::fs::copy(db_path, staging.join(staged_db_filename))?;

    // Copy known sidecars (skip missing ones silently).
    for suffix in SIDECAR_SUFFIXES {
        let sidecar = crate::sidecar_path(db_path, suffix);
        if !sidecar.exists() {
            continue;
        }
        let dest_name = {
            let mut s = staged_db_filename.to_owned();
            s.push(suffix);
            s
        };
        if sidecar.is_dir() {
            nestweaver_storage::copy_dir_all(&sidecar, &staging.join(&dest_name))?;
        } else {
            std::fs::copy(&sidecar, staging.join(&dest_name))?;
        }
    }

    // Copy WAL if it still exists (should be gone after CHECKPOINT, but be safe).
    let wal = crate::sidecar_path(db_path, ".wal");
    if wal.exists() {
        let mut wal_dest = staged_db_filename.to_owned();
        wal_dest.push(".wal");
        std::fs::copy(&wal, staging.join(&wal_dest))?;
    }

    // Copy instance.toml if present alongside the database.
    if let Some(parent) = db_path.parent() {
        let instance_toml = parent.join("instance.toml");
        if instance_toml.exists() {
            std::fs::copy(&instance_toml, staging.join("instance.toml"))?;
        }
    }

    // Full-tier: copy workspace .git directories.
    // Handles both regular clones (entry/<name>/.git/) and bare clones
    // (entry/<name>.git/ with HEAD file directly inside).
    if include_clones && let Some(ws) = workspace_path {
        let clones_dir = staging.join("clones");
        if ws.exists() {
            std::fs::create_dir_all(&clones_dir)?;
            for entry in std::fs::read_dir(ws)? {
                let entry = entry?;
                let path = entry.path();
                let git_dir = path.join(".git");
                if git_dir.is_dir() {
                    // Regular clone: copy only the .git subdirectory.
                    let dest = clones_dir.join(entry.file_name()).join(".git");
                    nestweaver_storage::copy_dir_all(&git_dir, &dest)?;
                } else if crate::bare_clone::BareClone::is_valid_at(&path) {
                    // Bare clone (e.g. <name>.git): the entry IS the git dir.
                    // Copy the entire directory.
                    let dest = clones_dir.join(entry.file_name());
                    nestweaver_storage::copy_dir_all(&path, &dest)?;
                }
            }
        }
    }

    Ok(())
}

/// Build the backup manifest by inspecting staged files.
fn build_backup_publication_bundle(
    config: &BackupConfig,
    staging: &Path,
    identity: &nestweaver_store::PublicationIdentity,
    source_graph_generation: u64,
) -> anyhow::Result<crate::publication::PublicationBundleV3> {
    identity
        .validate()
        .map_err(|error| anyhow::anyhow!("invalid backup publication identity: {error}"))?;
    let db_filename = config
        .db_path
        .file_name()
        .ok_or_else(|| anyhow::anyhow!("db_path has no filename"))?
        .to_string_lossy()
        .to_string();
    let mut entries: Vec<_> = walkdir::WalkDir::new(staging)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_file())
        .collect();
    entries.sort_by(|left, right| left.path().cmp(right.path()));
    let mut artifacts = Vec::with_capacity(entries.len());
    // nw-289. Two lists, because the two failure modes are not the same thing
    // and were being given one answer — a bare `?` that aborted the whole
    // bundle on the first problem.
    //
    // `excluded`: a DERIVED sidecar that describes an older generation of THIS
    // graph. It is rebuildable, the graph itself is healthy, and every reader
    // already refuses it — so it is dropped from the backup and named, not
    // treated as a reason to refuse the backup. This is the observed defect:
    // `backup save` failed 100% against a 5.6 GB production graph, `--force`
    // included, because `<db>.manifests.json` was two generations behind. It
    // is behind because NOTHING outside a code index rewrites it, while the
    // generation advances on every vault-watcher batch and every deletion
    // reconciliation (`bump_and_persist_graph_generation` is documented as
    // "the canonical call for the end of any graph-mutating operation"). A
    // read-only command cannot advance it, so it never self-heals.
    //
    // `fatal`: anything else — a foreign brain/publication identity, a
    // corrupt payload checksum, an unreadable file. Those say the artifact
    // does not belong to this graph, and shipping it would be worse than
    // refusing. They are COLLECTED rather than short-circuited, so one run
    // reports every problem instead of one per attempt.
    let mut excluded: Vec<String> = Vec::new();
    let mut fatal: Vec<String> = Vec::new();
    for entry in entries {
        let path = normalized_relative_path(staging, entry.path())?;
        if path == crate::publication::PUBLICATION_MANIFEST_FILE || path == "manifest.json" {
            continue;
        }
        let (kind, schema, fingerprint) = match backup_artifact_contract_for_path(
            &path,
            &db_filename,
            entry.path(),
            identity,
            source_graph_generation,
        ) {
            Ok(contract) => contract,
            Err(error) => {
                let message = format!("{error:#}");
                if nestweaver_store::artifact_envelope::is_stale_artifact_generation(&message) {
                    // Drop it from STAGING too, not just from the bundle: the
                    // archive inventory is exact, so a file present on disk
                    // and absent from the manifest fails verification.
                    std::fs::remove_file(entry.path()).with_context(|| {
                        format!("exclude stale artifact {}", entry.path().display())
                    })?;
                    excluded.push(format!("{path}: {message}"));
                } else {
                    fatal.push(format!("{path}: {message}"));
                }
                continue;
            }
        };
        let (byte_size, blake3) = crate::hash::blake3_file(entry.path())?;
        artifacts.push(crate::publication::ArtifactDescriptor {
            path,
            kind,
            artifact_schema_version: schema,
            byte_size,
            blake3,
            brain_uuid: identity.brain_uuid.clone(),
            publication_uuid: identity.publication_uuid.clone(),
            producer_version: env!("CARGO_PKG_VERSION").to_string(),
            source_graph_generation,
            algorithm_fingerprint: fingerprint,
        });
    }
    if !fatal.is_empty() {
        anyhow::bail!(
            "backup refused: {} artifact(s) do not belong to this graph:\n  {}",
            fatal.len(),
            fatal.join("\n  ")
        );
    }
    if !excluded.is_empty() {
        tracing::warn!(
            "backup excluded {} derived artifact(s) that describe an older graph \
             generation; the graph itself is backed up in full and these are \
             regenerated by the next index:\n  {}",
            excluded.len(),
            excluded.join("\n  ")
        );
    }
    let bundle = crate::publication::PublicationBundleV3 {
        format_version: crate::snapshot::SNAPSHOT_FORMAT_VERSION,
        brain_uuid: identity.brain_uuid.clone(),
        publication_uuid: identity.publication_uuid.clone(),
        producer_version: env!("CARGO_PKG_VERSION").to_string(),
        source_graph_generation,
        artifacts,
    };
    bundle.validate_metadata(crate::snapshot::SNAPSHOT_FORMAT_VERSION)?;
    Ok(bundle)
}

fn backup_artifact_contract_for_path(
    path: &str,
    db_filename: &str,
    absolute: &Path,
    identity: &nestweaver_store::PublicationIdentity,
    source_graph_generation: u64,
) -> anyhow::Result<(crate::publication::ArtifactKind, u32, String)> {
    backup_artifact_contract_for_path_inner(
        path,
        db_filename,
        absolute,
        identity,
        source_graph_generation,
    )
    // nw-289: the caller knows the file and the message did not name it, so a
    // user facing "stale artifact generation 93, expected 95" could not tell
    // WHICH of a dozen sidecars was the problem.
    .with_context(|| absolute.display().to_string())
}

fn backup_artifact_contract_for_path_inner(
    path: &str,
    db_filename: &str,
    absolute: &Path,
    identity: &nestweaver_store::PublicationIdentity,
    source_graph_generation: u64,
) -> anyhow::Result<(crate::publication::ArtifactKind, u32, String)> {
    match path.strip_prefix(db_filename) {
        Some(".pagerank.json") => {
            let payload = std::fs::read(absolute)?;
            let (schema, fingerprint) = crate::publication::pagerank_artifact_contract(
                &payload,
                identity,
                env!("CARGO_PKG_VERSION"),
                source_graph_generation,
            )?;
            return Ok((
                crate::publication::ArtifactKind::Ranking,
                schema,
                fingerprint,
            ));
        }
        Some(".manifests.json") => {
            let payload = std::fs::read(absolute)?;
            let (schema, fingerprint) = crate::publication::repo_manifest_artifact_contract(
                &payload,
                identity,
                env!("CARGO_PKG_VERSION"),
                source_graph_generation,
            )?;
            return Ok((
                crate::publication::ArtifactKind::RepoManifest,
                schema,
                fingerprint,
            ));
        }
        Some(".embeddings.bin") => {
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
                crate::publication::ArtifactKind::Embeddings,
                envelope.schema_version,
                envelope.algorithm_fingerprint()?,
            ));
        }
        Some(crate::publication::SOURCE_MANIFEST_SUFFIX) => {
            let payload = std::fs::read(absolute)?;
            let (schema, fingerprint) = crate::publication::source_manifest_artifact_contract(
                &payload,
                identity,
                env!("CARGO_PKG_VERSION"),
                source_graph_generation,
            )?;
            return Ok((
                crate::publication::ArtifactKind::SourceManifest,
                schema,
                fingerprint,
            ));
        }
        Some(crate::publication::PRESERVED_STATE_SUFFIX) => {
            let payload = std::fs::read(absolute)?;
            let (schema, fingerprint) = crate::publication::preserved_state_artifact_contract(
                &payload,
                identity,
                env!("CARGO_PKG_VERSION"),
                source_graph_generation,
            )?;
            return Ok((
                crate::publication::ArtifactKind::PreservedState,
                schema,
                fingerprint,
            ));
        }
        _ => {}
    }
    backup_artifact_contract(path, db_filename)
}

fn backup_artifact_contract(
    path: &str,
    db_filename: &str,
) -> anyhow::Result<(crate::publication::ArtifactKind, u32, String)> {
    use crate::publication::ArtifactKind;
    let suffix = path.strip_prefix(db_filename);
    let contract = if path == db_filename {
        (ArtifactKind::Graph, 1, "ladybugdb-graph-v1")
    } else if path == "instance.toml" {
        (ArtifactKind::InstanceConfig, 1, "instance-config-toml-v1")
    } else if path.starts_with("clones/") {
        (ArtifactKind::WorkspaceClone, 1, "git-object-store-v1")
    } else if path.starts_with(&format!("{db_filename}.tantivy/")) {
        (ArtifactKind::Bm25, 1, "nestweaver-tantivy-bm25-v1")
    } else if path.starts_with(&format!("{db_filename}.regex-v3/")) {
        (
            ArtifactKind::Regex,
            nestweaver_store::REGEX_INDEX_SCHEMA_VERSION,
            "nestweaver-regex-v3-unicode-scalar-trigrams-v1",
        )
    } else {
        match suffix {
            Some(".parsed_cache.bin") => {
                (ArtifactKind::ParsedCache, 1, "nestweaver-parsed-cache-v1")
            }
            Some(".resolution_deps.bin") => (
                ArtifactKind::ResolutionDependencies,
                1,
                "nestweaver-resolution-deps-v1",
            ),
            Some(crate::resolver_generation::RESOLVER_GENERATION_SIDECAR) => (
                ArtifactKind::CompatibilityStamp,
                1,
                "nestweaver-resolver-generation-v1",
            ),
            Some(".filemeta.json") => {
                (ArtifactKind::FileMetadata, 1, "nestweaver-file-metadata-v1")
            }
            Some(".manifests.json") => anyhow::bail!(
                "repository manifest contract requires payload inspection; use backup_artifact_contract_for_payload"
            ),
            // v2: repo-keyed. Bumped with the format, or a restore would
            // reintroduce a v1 flat payload under a v2 contract claim — the
            // artifact vouching for a shape it does not have.
            Some(".gitactivity.json") => {
                (ArtifactKind::GitActivity, 2, "nestweaver-git-activity-v2")
            }
            Some(".cochange.json") => (ArtifactKind::Cochange, 1, "nestweaver-cochange-v1"),
            Some(".interactions.json") => {
                (ArtifactKind::Interactions, 1, "nestweaver-interactions-v1")
            }
            Some(".extensions.json")
            | Some(".extensions.migration.json")
            | Some(".extensions.handoff.json") => (
                ArtifactKind::Extensions,
                1,
                "nestweaver-schema-extensions-v1",
            ),
            Some(".aliases.json") => (ArtifactKind::Aliases, 1, "nestweaver-aliases-v1"),
            Some(".bundles.json") => (ArtifactKind::Bundles, 1, "nestweaver-context-bundles-v1"),
            Some(".generation") => (
                ArtifactKind::Generation,
                1,
                "nestweaver-graph-generation-v1",
            ),
            Some(".embeddings.bin") => anyhow::bail!(
                "embedding contract requires payload inspection; use backup_artifact_contract_for_payload"
            ),
            Some(".embeddings") => (ArtifactKind::Embeddings, 1, "legacy-embedding-json-v1"),
            Some(crate::publication::SOURCE_MANIFEST_SUFFIX) => anyhow::bail!(
                "source manifest contract requires payload inspection; use backup_artifact_contract_for_payload"
            ),
            Some(crate::publication::PRESERVED_STATE_SUFFIX) => anyhow::bail!(
                "preserved-state contract requires payload inspection; use backup_artifact_contract_for_payload"
            ),
            Some(".pagerank.json") => anyhow::bail!(
                "PageRank contract requires payload inspection; use backup_artifact_contract_for_payload"
            ),
            Some(".wal") => (ArtifactKind::WriteAheadLog, 1, "ladybugdb-wal-v1"),
            _ => anyhow::bail!("unclassified backup artifact: {path}"),
        }
    };
    Ok((contract.0, contract.1, contract.2.to_string()))
}

fn build_backup_manifest(
    config: &BackupConfig,
    staging: &Path,
    repos: Vec<BackupRepoInfo>,
    symbol_count: usize,
    bundle: &crate::publication::PublicationBundleV3,
    publication_manifest_blake3: String,
) -> anyhow::Result<BackupManifest> {
    let db_filename = config
        .db_path
        .file_name()
        .ok_or_else(|| anyhow::anyhow!("db_path has no filename"))?
        .to_string_lossy()
        .to_string();

    // Compute SHA-256 of the database file (streamed; the DB is the largest file
    // in a backup and can be multiple GB).
    let db_staged = staging.join(&db_filename);
    let db_hash = sha256_stream_path(&db_staged)?;
    let db_size = std::fs::metadata(&db_staged)?.len();

    let mut checksums = HashMap::new();
    checksums.insert(db_filename.clone(), db_hash);

    // Checksum all sidecar files in the staging directory.
    checksum_sidecars(staging, &db_filename, &mut checksums)?;

    // Compute sizes for known sidecars.
    let tantivy_size = dir_size(&staging.join({
        let mut s = std::ffi::OsString::from(&db_filename);
        s.push(".tantivy");
        s
    }));

    let parsed_cache_size = staging
        .join({
            let mut s = std::ffi::OsString::from(&db_filename);
            s.push(".parsed_cache.bin");
            s
        })
        .metadata()
        .map(|m| m.len())
        .unwrap_or(0);

    let total_uncompressed = dir_size(staging);

    let tier = if config.include_clones {
        "full"
    } else {
        "standard"
    };

    // Timestamp in RFC 3339 UTC.
    let created_at = {
        use std::time::SystemTime;
        let now = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default();
        let secs = now.as_secs();
        // Manual RFC 3339 formatting to avoid pulling in chrono.
        let (y, mo, d, h, mi, s) = unix_to_utc(secs);
        format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z")
    };

    Ok(BackupManifest {
        version: MANIFEST_VERSION,
        tier: tier.to_string(),
        nestweaver_version: env!("CARGO_PKG_VERSION").to_string(),
        schema_version: 1,
        created_at,
        instance_id: config.instance_id.clone(),
        brain_uuid: bundle.brain_uuid.clone(),
        publication_uuid: bundle.publication_uuid.clone(),
        publication_manifest_blake3,
        repo_count: repos.len(),
        symbol_count,
        repos,
        sizes: BackupSizes {
            db: db_size,
            tantivy: tantivy_size,
            parsed_cache: parsed_cache_size,
            total_uncompressed,
            total_compressed: 0, // filled after packaging
        },
        checksums,
    })
}

/// Package a staging directory as tar + zstd.
fn package_tar_zstd(staging: &Path, output: &Path) -> anyhow::Result<()> {
    if let Some(parent) = output.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let file = std::fs::File::create(output)?;
    let encoder = nestweaver_store::zstd::Encoder::new(file, 3)?;
    let mut tar_builder = tar::Builder::new(encoder);
    tar_builder.append_dir_all(".", staging)?;
    let encoder = tar_builder.into_inner()?;
    encoder.finish()?;
    Ok(())
}

/// Verify SHA-256 checksums recorded in the manifest against the extracted files.
fn verify_backup_checksums(data_dir: &Path, manifest: &BackupManifest) -> anyhow::Result<()> {
    for (filename, expected_hash) in &manifest.checksums {
        let file_path = data_dir.join(filename);
        if !file_path.exists() {
            anyhow::bail!("checksum references missing file: {filename}");
        }
        let actual = sha256_stream_path(&file_path)?;
        if actual != *expected_hash {
            anyhow::bail!(
                "integrity check failed for {filename}: expected {expected_hash}, got {actual}"
            );
        }
    }
    Ok(())
}

fn validate_backup_publication_inventory(
    manifest: &BackupManifest,
    publication: &crate::publication::PublicationBundleV3,
    file_sizes: &HashMap<String, u64>,
    file_hashes: &HashMap<String, String>,
) -> anyhow::Result<()> {
    publication.validate_metadata(crate::snapshot::SNAPSHOT_FORMAT_VERSION)?;
    if publication.brain_uuid != manifest.brain_uuid
        || publication.publication_uuid != manifest.publication_uuid
    {
        anyhow::bail!(
            "backup summary identity {}/{} does not match publication {}/{}",
            manifest.brain_uuid,
            manifest.publication_uuid,
            publication.brain_uuid,
            publication.publication_uuid
        );
    }
    let publication_digest = file_hashes
        .get(crate::publication::PUBLICATION_MANIFEST_FILE)
        .ok_or_else(|| anyhow::anyhow!("publication.json is absent from backup inventory"))?;
    if publication_digest != &manifest.publication_manifest_blake3 {
        anyhow::bail!(
            "backup publication manifest digest mismatch: summary {}, file {publication_digest}",
            manifest.publication_manifest_blake3
        );
    }

    let described: std::collections::BTreeSet<_> = publication
        .artifacts
        .iter()
        .map(|artifact| artifact.path.as_str())
        .collect();
    if publication
        .artifacts
        .iter()
        .filter(|artifact| artifact.kind == crate::publication::ArtifactKind::Graph)
        .count()
        != 1
    {
        anyhow::bail!("backup publication must describe exactly one graph artifact");
    }
    let present: std::collections::BTreeSet<_> = file_hashes
        .keys()
        .filter(|path| {
            path.as_str() != "manifest.json"
                && path.as_str() != crate::publication::PUBLICATION_MANIFEST_FILE
        })
        .map(String::as_str)
        .collect();
    if described != present {
        anyhow::bail!(
            "backup publication does not exactly describe payloads (described {}, present {})",
            described.len(),
            present.len()
        );
    }
    for descriptor in &publication.artifacts {
        let size = file_sizes
            .get(&descriptor.path)
            .ok_or_else(|| anyhow::anyhow!("missing backup artifact {}", descriptor.path))?;
        let digest = file_hashes
            .get(&descriptor.path)
            .ok_or_else(|| anyhow::anyhow!("missing backup digest {}", descriptor.path))?;
        if *size != descriptor.byte_size || digest != &descriptor.blake3 {
            anyhow::bail!(
                "backup artifact {} does not match its descriptor (size {}/{}, digest {}/{})",
                descriptor.path,
                size,
                descriptor.byte_size,
                digest,
                descriptor.blake3
            );
        }
    }
    Ok(())
}

/// Check that the manifest version is compatible with this engine.
fn check_schema_compatibility(manifest: &BackupManifest) -> anyhow::Result<()> {
    if manifest.version > MANIFEST_VERSION {
        anyhow::bail!(
            "backup manifest version {} is newer than supported version {MANIFEST_VERSION} — \
             upgrade NestWeaver before restoring",
            manifest.version,
        );
    }
    Ok(())
}

/// Recursively compute the total size of all files in a directory.
/// Compute SHA-256 checksums for all sidecar files in the staging directory,
/// skipping the main database file (already checksummed by the caller).
fn checksum_sidecars(
    staging: &Path,
    db_filename: &str,
    checksums: &mut HashMap<String, String>,
) -> anyhow::Result<()> {
    for entry in walkdir::WalkDir::new(staging)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
    {
        let rel = normalized_relative_path(staging, entry.path())?;

        // Skip the db file — already checksummed.
        if rel == db_filename {
            continue;
        }
        // Skip the manifest itself if it's been written to staging.
        if rel == "manifest.json" {
            continue;
        }

        let hash = sha256_stream_path(entry.path())
            .with_context(|| format!("reading sidecar for checksum: {rel}"))?;
        checksums.insert(rel, hash);
    }
    Ok(())
}

fn normalized_relative_path(root: &Path, path: &Path) -> anyhow::Result<String> {
    let relative = path.strip_prefix(root)?;
    let mut components = Vec::new();
    for component in relative.components() {
        match component {
            std::path::Component::Normal(value) => {
                components.push(value.to_string_lossy().into_owned())
            }
            _ => anyhow::bail!("unsafe backup artifact path: {}", path.display()),
        }
    }
    if components.is_empty() {
        anyhow::bail!("backup artifact path is empty: {}", path.display());
    }
    Ok(components.join("/"))
}

fn dir_size(path: &Path) -> u64 {
    if !path.exists() {
        return 0;
    }
    if path.is_file() {
        return path.metadata().map(|m| m.len()).unwrap_or(0);
    }
    walkdir::WalkDir::new(path)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
        .map(|e| e.metadata().map(|m| m.len()).unwrap_or(0))
        .sum()
}

/// Convert a Unix timestamp to (year, month, day, hour, minute, second) in UTC.
fn unix_to_utc(secs: u64) -> (u64, u64, u64, u64, u64, u64) {
    let s = secs % 60;
    let total_min = secs / 60;
    let mi = total_min % 60;
    let total_hours = total_min / 60;
    let h = total_hours % 24;
    let mut days = total_hours / 24;

    // Compute year/month/day from days since epoch (1970-01-01).
    let mut y = 1970u64;
    loop {
        let days_in_year = if is_leap(y) { 366 } else { 365 };
        if days < days_in_year {
            break;
        }
        days -= days_in_year;
        y += 1;
    }

    let month_days: [u64; 12] = if is_leap(y) {
        [31, 29, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
    } else {
        [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
    };

    let mut mo = 1u64;
    for &md in &month_days {
        if days < md {
            break;
        }
        days -= md;
        mo += 1;
    }
    let d = days + 1;

    (y, mo, d, h, mi, s)
}

/// Stream a reader through SHA-256 in fixed-size chunks — a backup's DB/index can
/// be multiple GB, so reading the whole file into a `Vec` before hashing would
/// spike memory to the file size. Returns the `sha256:<hex>` form.
fn sha256_stream(mut reader: impl std::io::Read) -> std::io::Result<String> {
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 65536];
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(format!("sha256:{}", hex_encode(&hasher.finalize())))
}

/// Compute the legacy backup SHA-256 and publication BLAKE3 digests in one
/// streaming pass.
fn hash_stream_both(mut reader: impl std::io::Read) -> std::io::Result<(String, String)> {
    let mut sha256 = Sha256::new();
    let mut blake3 = blake3::Hasher::new();
    let mut buffer = [0_u8; 65_536];
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        sha256.update(&buffer[..read]);
        blake3.update(&buffer[..read]);
    }
    Ok((
        format!("sha256:{}", hex_encode(&sha256.finalize())),
        blake3.finalize().to_hex().to_string(),
    ))
}

/// Convenience: stream-hash a file at `path` (see [`sha256_stream`]).
fn sha256_stream_path(path: impl AsRef<Path>) -> std::io::Result<String> {
    sha256_stream(std::fs::File::open(path)?)
}

/// Encode bytes as lowercase hex string.
fn hex_encode(bytes: &[u8]) -> String {
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut s, b| {
            use std::fmt::Write;
            let _ = write!(s, "{b:02x}");
            s
        })
}

fn is_leap(y: u64) -> bool {
    (y.is_multiple_of(4) && !y.is_multiple_of(100)) || y.is_multiple_of(400)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn backup_symbol(uid: &str, name: &str) -> nestweaver_schema::Symbol {
        nestweaver_schema::Symbol {
            uid: uid.into(),
            name: name.into(),
            kind: nestweaver_schema::SymbolKind::Function,
            repo_uid: "repo:backup-publication".into(),
            file_path: "src/backup.rs".into(),
            start_line: 1,
            end_line: 2,
            signature: format!("fn {name}()"),
            summary: None,
            content_hash: format!("hash:{uid}"),
            embedding: None,
            pagerank_score: None,
            is_entry_point: false,
            entry_point_kind: None,
            visibility: nestweaver_schema::Visibility::Inferred,
            type_info: None,
            framework_hint: None,
            canonical_id: None,
        }
    }

    #[test]
    fn test_backup_save_and_inspect() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.lbug");
        let store = nestweaver_store::GraphStore::create(&db_path).unwrap();
        drop(store);

        let output = dir.path().join("test.nwsnap.zst");
        let config = BackupConfig {
            db_path: db_path.clone(),
            output_path: output.clone(),
            include_clones: false,
            instance_id: "test".to_string(),
            workspace_path: None,
        };

        let result = backup_save(&config).unwrap();
        assert!(output.exists());
        assert_eq!(result.manifest.instance_id, "test");
        assert_eq!(result.manifest.tier, "standard");

        let manifest = backup_inspect(&output).unwrap();
        assert_eq!(manifest.instance_id, "test");
        assert_eq!(manifest.version, MANIFEST_VERSION);
        assert!(uuid::Uuid::parse_str(&manifest.brain_uuid).is_ok());
        assert!(uuid::Uuid::parse_str(&manifest.publication_uuid).is_ok());
        assert_eq!(manifest.publication_manifest_blake3.len(), 64);
        assert!(
            manifest
                .checksums
                .contains_key(crate::publication::PUBLICATION_MANIFEST_FILE)
        );
    }

    #[test]
    fn backup_inspect_ignores_nested_directory_members_in_exact_inventory() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.lbug");
        let store = nestweaver_store::GraphStore::create(&db_path).unwrap();
        drop(store);

        // Regex v3 is the first routinely nested backup sidecar. `tar` writes
        // these directory members without a required trailing slash, which
        // must not make inspect count them as publication payloads.
        let shard_file = crate::sidecar_path(&db_path, ".regex-v3")
            .join("scopes/scope-a/generations/generation-a/meta.json");
        std::fs::create_dir_all(shard_file.parent().unwrap()).unwrap();
        std::fs::write(&shard_file, b"{}\n").unwrap();

        let output = dir.path().join("nested.nwsnap.zst");
        let config = BackupConfig {
            db_path,
            output_path: output.clone(),
            include_clones: false,
            instance_id: "nested".to_string(),
            workspace_path: None,
        };

        backup_save(&config).unwrap();
        let manifest = backup_inspect(&output).unwrap();
        assert_eq!(manifest.instance_id, "nested");
        assert!(
            manifest.checksums.contains_key(
                "test.lbug.regex-v3/scopes/scope-a/generations/generation-a/meta.json"
            )
        );
    }

    #[test]
    fn backup_archive_member_contract_rejects_links_and_special_entries() {
        assert!(!backup_archive_member_is_payload("nested", tar::EntryType::Directory).unwrap());
        assert!(backup_archive_member_is_payload("payload", tar::EntryType::Regular).unwrap());
        for entry_type in [
            tar::EntryType::Symlink,
            tar::EntryType::Link,
            tar::EntryType::Char,
            tar::EntryType::Block,
            tar::EntryType::Fifo,
        ] {
            let error = backup_archive_member_is_payload("unsafe", entry_type).unwrap_err();
            assert!(
                error
                    .to_string()
                    .contains("unsupported backup archive member type")
            );
        }
    }

    /// nw-289 / F-EXPORT-2. `backup save` failed 100% against the real
    /// production graph with "stale artifact generation 93, expected 95",
    /// because `<db>.manifests.json` is stamped with the generation that was
    /// current at the moment it was written, while the index publication
    /// advances the graph generation around it. Every sidecar written by one
    /// index must describe the same graph, or the artifact contract that
    /// backup enforces refuses a perfectly healthy database.
    ///
    /// Temp dir and temp DB only.
    #[test]
    fn every_sidecar_written_by_an_index_describes_the_published_generation() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::write(repo.join("a.py"), "def f():\n    return 1\n").unwrap();
        let db_path = dir.path().join("graph.lbug");

        crate::index::index_directory(&repo, &db_path, "test", "file:///repo", "sha")
            .expect("index the fixture repo");

        let stamped = |suffix: &str| -> Option<u64> {
            let path = crate::sidecar_path(&db_path, suffix);
            let bytes = std::fs::read(path).ok()?;
            let value: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
            value["source_graph_generation"].as_u64()
        };
        let published: u64 = std::fs::read_to_string(crate::sidecar_path(&db_path, ".generation"))
            .expect("the index must persist a generation sidecar")
            .trim()
            .parse()
            .expect("the generation sidecar must hold a number");

        assert_eq!(
            stamped(".manifests.json"),
            Some(published),
            "nw-289: the manifest sidecar is stale the moment the index returns \
             (pagerank is at {:?}, the published graph is at {published})",
            stamped(".pagerank.json"),
        );
        assert_eq!(
            stamped(".pagerank.json"),
            Some(published),
            "the pagerank sidecar must describe the published generation too"
        );

        // And the observable consequence the user reported: backup succeeds.
        let output = dir.path().join("out.nwsnap.zst");
        let result = backup_save(&BackupConfig {
            db_path: db_path.clone(),
            output_path: output.clone(),
            include_clones: false,
            instance_id: "test".to_string(),
            workspace_path: None,
        });
        assert!(
            result.is_ok(),
            "backup refused a freshly indexed database: {:?}",
            result.err()
        );
        assert!(output.exists());
    }

    /// nw-289 / F-EXPORT-2, the defect as it actually reproduces.
    ///
    /// `backup save` failed 100% against the production graph with
    /// `stale artifact generation 93, expected 95`, `--force` included, and
    /// nothing could heal it but another code index.
    ///
    /// The dossier attributed this to the index epilogue stamping the manifest
    /// sidecar before advancing the generation. That does NOT reproduce — see
    /// `every_sidecar_written_by_an_index_describes_the_published_generation`,
    /// which passed before any change here: one index leaves `.manifests.json`,
    /// `.pagerank.json` and `.generation` in agreement. The real mechanism is
    /// the one this test pins: NOTHING outside a code index rewrites
    /// `.manifests.json`, while `bump_and_persist_graph_generation` — "the
    /// canonical call for the end of any graph-mutating operation", i.e. every
    /// vault-watcher batch and every deletion reconciliation — advances the
    /// generation around it. The sidecar is then permanently behind, and a
    /// read-only command cannot advance it, so there is no self-heal.
    ///
    /// Backing up a healthy 5.6 GB graph must not be blocked by a rebuildable
    /// cache being behind.
    #[test]
    fn backup_survives_a_derived_sidecar_left_behind_by_a_later_generation() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::write(repo.join("a.py"), "def f():\n    return 1\n").unwrap();
        let db_path = dir.path().join("graph.lbug");
        crate::index::index_directory(&repo, &db_path, "test", "file:///repo", "sha").unwrap();

        let manifests = crate::manifest::manifest_cache_path(&db_path);
        assert!(
            manifests.exists(),
            "the fixture must have a manifest sidecar"
        );

        // Exactly what a vault note edit does to this database.
        {
            let store = nestweaver_store::GraphStore::open(&db_path).unwrap();
            store.bump_and_persist_graph_generation(&crate::sidecar_path(&db_path, ".generation"));
        }

        let output = dir.path().join("out.nwsnap.zst");
        let result = backup_save(&BackupConfig {
            db_path: db_path.clone(),
            output_path: output.clone(),
            include_clones: false,
            instance_id: "test".to_string(),
            workspace_path: None,
        });
        assert!(
            result.is_ok(),
            "backup refused a healthy graph because a rebuildable cache was one \
             generation behind: {:?}",
            result.err()
        );
        assert!(output.exists());

        // The graph itself — the thing being protected — is in the archive.
        let inspected = backup_inspect(&output).unwrap();
        assert!(
            inspected.checksums.contains_key("graph.lbug"),
            "the database must be backed up in full: {:?}",
            inspected.checksums.keys().collect::<Vec<_>>()
        );
        // …and the stale derived artifact is not, so the archive inventory
        // stays exact and a restore cannot resurrect it.
        assert!(
            !inspected
                .checksums
                .contains_key("graph.lbug.manifests.json"),
            "a stale derived artifact must be excluded, not shipped: {:?}",
            inspected.checksums.keys().collect::<Vec<_>>()
        );
    }

    /// nw-289, defect (2), separated as the finding asks: when a guard DOES
    /// refuse, the message must meet the standard the PageRank guard already
    /// meets — name the artifact and name the fix. It said only
    /// `stale artifact generation 93, expected 95`: two numbers, no artifact,
    /// no file, no remedy.
    #[test]
    fn a_stale_artifact_is_refused_by_name_with_a_remedy() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::write(repo.join("a.py"), "def f():\n    return 1\n").unwrap();
        let db_path = dir.path().join("graph.lbug");
        crate::index::index_directory(&repo, &db_path, "test", "file:///repo", "sha").unwrap();

        let store = nestweaver_store::GraphStore::open(&db_path).unwrap();
        store.bump_and_persist_graph_generation(&crate::sidecar_path(&db_path, ".generation"));

        // The read path still refuses — correctly, it cannot trust the payload.
        let error = crate::manifest::load_manifest_cache_for_db(&store, &db_path)
            .expect_err("a stale generation-bound artifact must not be trusted");
        let message = format!("{error:#}");

        assert!(
            message.contains("repo_manifest"),
            "the error must name WHICH artifact is stale: {message}"
        );
        assert!(
            message.contains("re-index"),
            "the error must name a fix, as the PageRank guard already does: {message}"
        );
        assert!(
            nestweaver_store::artifact_envelope::is_stale_artifact_generation(&message),
            "and it must be classifiable, so a caller can tell 'older generation \
             of this graph' from 'belongs to another graph': {message}"
        );
    }

    #[test]
    fn stage_and_package_from_open_store_produces_snapshot() {
        // The daemon backs up while its write connection is OPEN. Staging must
        // work against a live store (checkpoint + copy + stats) and packaging
        // must run afterwards — without opening a second connection.
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.lbug");
        let store = nestweaver_store::GraphStore::create(&db_path).unwrap();

        let output = dir.path().join("snap.nwsnap.zst");
        let config = BackupConfig {
            db_path: db_path.clone(),
            output_path: output.clone(),
            include_clones: false,
            instance_id: "test".to_string(),
            workspace_path: None,
        };

        let staged = stage_backup_from_store(&store, &config).expect("stage");
        let result = package_staged(&config, staged).expect("package");
        // Store is still open here — the daemon keeps serving.
        assert!(store.count_symbols().is_ok());
        assert!(output.exists());
        assert_eq!(result.manifest.instance_id, "test");
        assert_eq!(backup_inspect(&output).unwrap().instance_id, "test");
    }

    #[test]
    fn stage_rejects_store_and_config_database_mismatch() {
        let dir = tempfile::tempdir().unwrap();
        let store_a_path = dir.path().join("a.lbug");
        let store_b_path = dir.path().join("b.lbug");
        let store_a = nestweaver_store::GraphStore::create(&store_a_path).unwrap();
        let store_b = nestweaver_store::GraphStore::create(&store_b_path).unwrap();
        let dirty_b = crate::index::establish_index_publication_marker_with_io(
            &store_b,
            Some(&store_b_path),
            "mismatched backup source",
            &crate::index::FileSystemIndexEpilogueIo,
        )
        .unwrap();
        store_b
            .insert_symbol(&backup_symbol("sym:backup:mismatched", "mismatched"))
            .unwrap();

        let config = BackupConfig {
            db_path: store_b_path.clone(),
            output_path: dir.path().join("must-not-exist.nwsnap.zst"),
            include_clones: false,
            instance_id: "test".into(),
            workspace_path: None,
        };
        let error = match stage_backup_from_store(&store_a, &config) {
            Ok(_) => panic!("mismatched store and config unexpectedly produced a staged backup"),
            Err(error) => error,
        };

        assert!(error.to_string().contains("does not match"));
        assert!(
            crate::sidecar_path(&store_b_path, ".index-dirty").exists(),
            "rejecting the mismatch must not alter the actual config database"
        );
        assert!(store_b.is_index_publication_dirty());
        drop(dirty_b);
    }

    #[test]
    fn stage_accepts_relative_path_to_the_opened_store() {
        let current_dir = std::env::current_dir().unwrap();
        let dir = tempfile::tempdir_in(&current_dir).unwrap();
        let store_path = dir.path().join("relative.lbug");
        let relative_path = store_path.strip_prefix(&current_dir).unwrap().to_path_buf();
        let store = nestweaver_store::GraphStore::create(&store_path).unwrap();
        let config = BackupConfig {
            db_path: relative_path,
            output_path: dir.path().join("relative.nwsnap.zst"),
            include_clones: false,
            instance_id: "test".into(),
            workspace_path: None,
        };

        let staged = stage_backup_from_store(&store, &config).unwrap();
        let result = package_staged(&config, staged).unwrap();
        assert_eq!(result.manifest.repo_count, 0);
    }

    #[cfg(unix)]
    #[test]
    fn stage_accepts_symlink_equivalent_path_and_copies_store_sidecars() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let store_path = dir.path().join("real.lbug");
        let alias_path = dir.path().join("alias.lbug");
        let store = nestweaver_store::GraphStore::create(&store_path).unwrap();
        let store_sidecar = crate::sidecar_path(&store_path, ".aliases.json");
        std::fs::write(&store_sidecar, r#"{"sentinel":"from-store"}"#).unwrap();
        symlink(&store_path, &alias_path).unwrap();

        let output = dir.path().join("alias.nwsnap.zst");
        let config = BackupConfig {
            db_path: alias_path,
            output_path: output,
            include_clones: false,
            instance_id: "test".into(),
            workspace_path: None,
        };
        let staged = stage_backup_from_store(&store, &config).unwrap();
        let result = package_staged(&config, staged).unwrap();

        assert!(
            result
                .manifest
                .checksums
                .contains_key("alias.lbug.aliases.json"),
            "an equivalent config alias must stage sidecars located beside the store path"
        );
    }

    #[test]
    fn backup_waits_for_active_publication_and_restores_complete_latest_generation() {
        use std::sync::Arc;

        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.lbug");
        let output = dir.path().join("snap.nwsnap.zst");
        let restore_dir = dir.path().join("restored");
        let config = BackupConfig {
            db_path: db_path.clone(),
            output_path: output.clone(),
            include_clones: false,
            instance_id: "test".into(),
            workspace_path: None,
        };
        let store = Arc::new(nestweaver_store::GraphStore::open_or_create(&db_path).unwrap());

        let publication = crate::index::establish_index_publication_marker_with_io(
            &store,
            Some(&db_path),
            "paused watcher publication",
            &crate::index::FileSystemIndexEpilogueIo,
        )
        .unwrap();
        store
            .insert_repo(&nestweaver_schema::Repo {
                uid: "repo:backup-publication".into(),
                url: "https://example.test/backup-publication".into(),
                indexed_sha: "latest".into(),
                staleness_commits_behind: 0,
                instance_id: "test".into(),
                name: None,
                root_path: None,
            })
            .unwrap();
        store
            .insert_symbol(&backup_symbol("sym:backup:first", "first"))
            .unwrap();

        let backup_store = Arc::clone(&store);
        let backup_config = config.clone();
        let backup_thread =
            std::thread::spawn(move || stage_backup_from_store(&backup_store, &backup_config));
        assert!(
            store.wait_for_index_publication_waiters(1, Duration::from_secs(2)),
            "backup must register as waiting before checkpoint/copy"
        );

        store
            .insert_symbol(&backup_symbol("sym:backup:second", "second"))
            .unwrap();
        store
            .insert_edge(&nestweaver_schema::ResolvedEdge {
                source_uid: "sym:backup:first".into(),
                target_uid: "sym:backup:second".into(),
                edge_type: nestweaver_schema::EdgeType::Calls,
                confidence: 1.0,
                link_type: None,
                evidence: Vec::new(),
            })
            .unwrap();
        crate::index::finalize_committed_index_for_scope_with_io(
            publication,
            Some(&db_path),
            "paused watcher publication",
            &crate::index::FileSystemIndexEpilogueIo,
            Some(&nestweaver_store::GraphScope::code_only()),
            true,
        )
        .unwrap();
        let latest_generation = store.graph_generation();

        let staged = backup_thread.join().unwrap().unwrap();
        package_staged(&config, staged).unwrap();
        backup_restore(&RestoreConfig {
            snapshot_path: output,
            data_dir: restore_dir.clone(),
        })
        .unwrap();

        let restored =
            nestweaver_store::GraphStore::open_or_create(&restore_dir.join("test.lbug")).unwrap();
        assert_eq!(restored.graph_generation(), latest_generation);
        assert!(restored.lookup_symbol("sym:backup:first").is_ok());
        assert!(restored.lookup_symbol("sym:backup:second").is_ok());
        assert_eq!(
            restored
                .callees_of("sym:backup:first")
                .unwrap()
                .into_iter()
                .map(|symbol| symbol.uid)
                .collect::<Vec<_>>(),
            vec!["sym:backup:second".to_string()]
        );
    }

    #[test]
    fn backup_rejects_abandoned_dirty_publication_and_preserves_marker() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.lbug");
        let marker_path = crate::sidecar_path(&db_path, ".index-dirty");
        let config = BackupConfig {
            db_path: db_path.clone(),
            output_path: dir.path().join("must-not-exist.nwsnap.zst"),
            include_clones: false,
            instance_id: "test".into(),
            workspace_path: None,
        };
        let store = nestweaver_store::GraphStore::open_or_create(&db_path).unwrap();
        let abandoned = crate::index::establish_index_publication_marker_with_io(
            &store,
            Some(&db_path),
            "abandoned watcher publication",
            &crate::index::FileSystemIndexEpilogueIo,
        )
        .unwrap();
        store
            .insert_symbol(&backup_symbol("sym:backup:abandoned", "abandoned"))
            .unwrap();
        drop(abandoned);

        let error = match stage_backup_from_store(&store, &config) {
            Ok(_) => panic!("dirty publication backup unexpectedly succeeded"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("dirty index publication"));
        assert!(marker_path.exists());
        assert!(store.is_index_publication_dirty());
    }

    #[test]
    fn restore_recovers_from_leftover_restoring_dir() {
        // Prior restore crashed after moving data aside but before the new data
        // landed: data dir is gone, `.restoring` holds the only surviving copy.
        let tmp = tempfile::TempDir::new().unwrap();
        let data = tmp.path().join("data");
        let restoring = tmp.path().join("data.restoring");
        std::fs::create_dir_all(&restoring).unwrap();
        std::fs::write(restoring.join("brain.lbug"), b"RECOVERED").unwrap();

        recover_interrupted_restore(&data).expect("recover");

        assert_eq!(
            std::fs::read(data.join("brain.lbug")).unwrap(),
            b"RECOVERED",
            "the only surviving copy must be recovered into place"
        );
        assert!(
            !restoring.exists(),
            ".restoring must be consumed by the recovery, not left behind"
        );
    }

    #[test]
    fn restore_removes_orphan_restoring_when_data_present() {
        // Prior restore completed the swap but crashed before deleting
        // `.restoring`: the data dir is intact, `.restoring` is a stale orphan.
        let tmp = tempfile::TempDir::new().unwrap();
        let data = tmp.path().join("data");
        std::fs::create_dir_all(&data).unwrap();
        std::fs::write(data.join("brain.lbug"), b"CURRENT").unwrap();
        let restoring = tmp.path().join("data.restoring");
        std::fs::create_dir_all(&restoring).unwrap();
        std::fs::write(restoring.join("stale"), b"x").unwrap();

        recover_interrupted_restore(&data).expect("recover");

        assert!(!restoring.exists(), "orphan .restoring must be removed");
        assert_eq!(
            std::fs::read(data.join("brain.lbug")).unwrap(),
            b"CURRENT",
            "current data must be left untouched"
        );
    }

    #[test]
    fn manifest_reports_real_counts_and_compressed_size() {
        use nestweaver_schema::{Repo, Symbol, SymbolKind, Visibility};

        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.lbug");
        let store = nestweaver_store::GraphStore::create(&db_path).unwrap();
        store
            .insert_repo(&Repo {
                uid: "repo-1".to_string(),
                url: "https://github.com/acme/api".to_string(),
                indexed_sha: "deadbeef".to_string(),
                staleness_commits_behind: 0,
                instance_id: "test".to_string(),
                name: None,
                root_path: None,
            })
            .unwrap();
        for (i, name) in ["alpha", "beta", "gamma"].iter().enumerate() {
            store
                .insert_symbol(&Symbol {
                    uid: format!("sym-{i}"),
                    name: name.to_string(),
                    kind: SymbolKind::Function,
                    repo_uid: "repo-1".to_string(),
                    file_path: format!("src/{name}.rs"),
                    start_line: 1,
                    end_line: 2,
                    signature: format!("fn {name}()"),
                    summary: None,
                    content_hash: format!("h{i}"),
                    embedding: None,
                    pagerank_score: None,
                    is_entry_point: false,
                    entry_point_kind: None,
                    visibility: Visibility::Inferred,
                    type_info: None,
                    framework_hint: None,
                    canonical_id: None,
                })
                .unwrap();
        }
        drop(store);

        let output = dir.path().join("test.nwsnap.zst");
        let config = BackupConfig {
            db_path: db_path.clone(),
            output_path: output.clone(),
            include_clones: false,
            instance_id: "test".to_string(),
            workspace_path: None,
        };

        let result = backup_save(&config).unwrap();
        // Headline counts must reflect the real graph, not hardcoded zeros.
        assert_eq!(result.manifest.repo_count, 1, "repo_count");
        assert_eq!(result.manifest.symbol_count, 3, "symbol_count");
        assert_eq!(result.manifest.repos.len(), 1);
        assert_eq!(result.manifest.repos[0].symbols, 3, "per-repo symbol count");
        assert!(
            result.manifest.sizes.total_compressed > 0,
            "compressed size must be reported on the save result"
        );

        // Inspect (reading the sealed archive) must also report a real
        // compressed size and the same counts.
        let inspected = backup_inspect(&output).unwrap();
        assert_eq!(inspected.repo_count, 1);
        assert_eq!(inspected.symbol_count, 3);
        assert!(
            inspected.sizes.total_compressed > 0,
            "inspect must recompute compressed size from the archive"
        );
    }

    #[test]
    fn test_backup_save_restore_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.lbug");
        let store = nestweaver_store::GraphStore::create(&db_path).unwrap();
        drop(store);

        let output = dir.path().join("test.nwsnap.zst");
        let config = BackupConfig {
            db_path: db_path.clone(),
            output_path: output.clone(),
            include_clones: false,
            instance_id: "test".to_string(),
            workspace_path: None,
        };

        backup_save(&config).unwrap();

        let restore_dir = dir.path().join("restored");
        let restore_config = RestoreConfig {
            snapshot_path: output,
            data_dir: restore_dir.clone(),
        };

        let result = backup_restore(&restore_config).unwrap();
        assert_eq!(result.manifest.instance_id, "test");

        let restored_db = restore_dir.join("test.lbug");
        let store = nestweaver_store::GraphStore::open_read_only(&restored_db).unwrap();
        assert!(store.db_path().is_some());
    }

    #[test]
    fn test_backup_list_finds_snapshots() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.lbug");
        let store = nestweaver_store::GraphStore::create(&db_path).unwrap();
        drop(store);

        let list_dir = dir.path().join("backups");
        std::fs::create_dir_all(&list_dir).unwrap();

        for name in &["a.nwsnap.zst", "b.nwsnap.zst"] {
            let config = BackupConfig {
                db_path: db_path.clone(),
                output_path: list_dir.join(name),
                include_clones: false,
                instance_id: "test".to_string(),
                workspace_path: None,
            };
            backup_save(&config).unwrap();
        }

        let items = backup_list(&list_dir).unwrap();
        assert_eq!(items.len(), 2);
    }

    #[test]
    fn test_corrupted_checksum_detected() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.lbug");
        let store = nestweaver_store::GraphStore::create(&db_path).unwrap();
        drop(store);

        let output = dir.path().join("test.nwsnap.zst");
        let config = BackupConfig {
            db_path: db_path.clone(),
            output_path: output.clone(),
            include_clones: false,
            instance_id: "test".to_string(),
            workspace_path: None,
        };
        backup_save(&config).unwrap();

        let restore_dir = dir.path().join("restored");
        let restore_config = RestoreConfig {
            snapshot_path: output,
            data_dir: restore_dir.clone(),
        };
        backup_restore(&restore_config).unwrap();

        // Corrupt the db file.
        std::fs::write(restore_dir.join("test.lbug"), b"corrupted").unwrap();

        let manifest_str = std::fs::read_to_string(restore_dir.join("manifest.json")).unwrap();
        let manifest: BackupManifest = serde_json::from_str(&manifest_str).unwrap();
        let err = verify_backup_checksums(&restore_dir, &manifest).unwrap_err();
        assert!(err.to_string().contains("integrity check failed"));
    }

    #[test]
    fn test_missing_sidecars_dont_error() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("bare.lbug");
        let store = nestweaver_store::GraphStore::create(&db_path).unwrap();
        drop(store);

        // Delete any auto-created sidecars.
        for suffix in SIDECAR_SUFFIXES {
            let sidecar = crate::sidecar_path(&db_path, suffix);
            let _ = std::fs::remove_file(&sidecar);
            let _ = std::fs::remove_dir_all(&sidecar);
        }

        let output = dir.path().join("bare.nwsnap.zst");
        let config = BackupConfig {
            db_path: db_path.clone(),
            output_path: output.clone(),
            include_clones: false,
            instance_id: "bare-test".to_string(),
            workspace_path: None,
        };

        let result = backup_save(&config).unwrap();
        assert!(output.exists());
        assert_eq!(result.manifest.instance_id, "bare-test");
    }

    #[test]
    fn backup_sidecars_include_incomplete_extension_migration_journal() {
        assert!(SIDECAR_SUFFIXES.contains(&".extensions.migration.json"));
        assert!(SIDECAR_SUFFIXES.contains(&".extensions.handoff.json"));
        assert!(
            SIDECAR_SUFFIXES.contains(&crate::resolver_generation::RESOLVER_GENERATION_SIDECAR)
        );
    }

    #[test]
    fn live_publication_slot_is_sealed_last_with_graph_identity() {
        let dir = tempfile::tempdir().unwrap();
        let slot = dir.path().join("slot");
        std::fs::create_dir(&slot).unwrap();
        let db_path = slot.join(crate::publication::PUBLICATION_GRAPH_FILE);
        let store = nestweaver_store::GraphStore::create(&db_path).unwrap();
        let identity = store.publication_identity().unwrap().unwrap();
        drop(store);

        let bundle = seal_publication_slot(&db_path, &slot).unwrap();
        assert_eq!(bundle.brain_uuid, identity.brain_uuid);
        assert_eq!(bundle.publication_uuid, identity.publication_uuid);
        assert!(bundle.artifacts.iter().any(|artifact| {
            artifact.kind == crate::publication::ArtifactKind::Graph
                && artifact.path == crate::publication::PUBLICATION_GRAPH_FILE
        }));
        let persisted: crate::publication::PublicationBundleV3 = serde_json::from_slice(
            &std::fs::read(slot.join(crate::publication::PUBLICATION_MANIFEST_FILE)).unwrap(),
        )
        .unwrap();
        assert_eq!(persisted, bundle);
    }

    #[test]
    fn test_schema_compatibility_rejects_newer() {
        let manifest = BackupManifest {
            version: MANIFEST_VERSION + 1,
            tier: "standard".to_string(),
            nestweaver_version: "99.0.0".to_string(),
            schema_version: 1,
            created_at: "2026-01-01T00:00:00Z".to_string(),
            instance_id: "future".to_string(),
            brain_uuid: String::new(),
            publication_uuid: String::new(),
            publication_manifest_blake3: String::new(),
            repos: Vec::new(),
            repo_count: 0,
            symbol_count: 0,
            sizes: BackupSizes {
                db: 0,
                tantivy: 0,
                parsed_cache: 0,
                total_uncompressed: 0,
                total_compressed: 0,
            },
            checksums: HashMap::new(),
        };
        let err = check_schema_compatibility(&manifest).unwrap_err();
        assert!(err.to_string().contains("newer than supported"));
    }

    #[test]
    fn test_unix_to_utc_epoch() {
        let (y, mo, d, h, mi, s) = unix_to_utc(0);
        assert_eq!((y, mo, d, h, mi, s), (1970, 1, 1, 0, 0, 0));
    }

    #[test]
    fn test_unix_to_utc_known_date() {
        // 2026-06-25T12:30:45Z = 1782382245
        let (y, mo, _d, _h, _mi, _s) = unix_to_utc(1_782_382_245);
        assert_eq!(y, 2026);
        assert_eq!(mo, 6);
        // Exact day/time verified by calculation.
    }
}
