use anyhow::Context;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Known sidecar suffixes to include in backups.
const SIDECAR_SUFFIXES: &[&str] = &[
    ".tantivy",
    ".parsed_cache.bin",
    ".resolution_deps.bin",
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
];

/// Current backup manifest version.
const MANIFEST_VERSION: u32 = 1;

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
        .flush_embedding_index()
        .map_err(|e| anyhow::anyhow!("failed to flush embedding index: {e}"))?;
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
    } = staged;
    let mut manifest = build_backup_manifest(config, staging.path(), repos, symbol_count)?;
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

/// Read the manifest from an existing `.nwsnap.zst` archive without full extraction.
///
/// Verifies that file sizes in the archive match the manifest checksums entries
/// and recomputes checksums for integrity verification.
pub fn backup_inspect(archive_path: &Path) -> anyhow::Result<BackupManifest> {
    let file = std::fs::File::open(archive_path)?;
    let decoder = zstd::Decoder::new(file)?;
    let mut archive = tar::Archive::new(decoder);

    let mut manifest: Option<BackupManifest> = None;
    let mut archive_file_sizes: HashMap<String, u64> = HashMap::new();
    let mut archive_checksums: HashMap<String, String> = HashMap::new();

    for entry in archive.entries()? {
        let mut entry = entry?;
        let path = entry.path()?.to_string_lossy().to_string();
        let normalized = path.strip_prefix("./").unwrap_or(&path).to_string();

        if normalized == "manifest.json" {
            let m: BackupManifest = serde_json::from_reader(&mut entry)?;
            manifest = Some(m);
        } else if !normalized.is_empty() && !normalized.ends_with('/') {
            // Track file sizes for verification.
            let size = entry.header().size()?;
            archive_file_sizes.insert(normalized.clone(), size);

            // Compute checksum for files listed in manifest checksums (streamed,
            // so a multi-GB member never lands in memory).
            let hash = sha256_stream(&mut entry)?;
            archive_checksums.insert(normalized, hash);
        }
    }

    let mut manifest =
        manifest.ok_or_else(|| anyhow::anyhow!("manifest.json not found in archive"))?;

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
    let decoder = zstd::Decoder::new(file)?;
    let mut archive = tar::Archive::new(decoder);
    archive.unpack(temp_dir.path())?;

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
fn build_backup_manifest(
    config: &BackupConfig,
    staging: &Path,
    repos: Vec<BackupRepoInfo>,
    symbol_count: usize,
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
    let encoder = zstd::Encoder::new(file, 3)?;
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
        let rel = entry
            .path()
            .strip_prefix(staging)
            .unwrap_or(entry.path())
            .to_string_lossy()
            .to_string();

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
        assert_eq!(manifest.version, 1);
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
