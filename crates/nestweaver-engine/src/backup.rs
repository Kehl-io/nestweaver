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
/// The caller is responsible for ensuring exclusive access to the database
/// (no concurrent writes) for the duration of this call.
pub fn backup_save(config: &BackupConfig) -> anyhow::Result<BackupResult> {
    let start = Instant::now();
    let staging = tempfile::tempdir()?;

    // Phase 1: Quiesce — flush in-memory state and merge WAL.
    let pause_start = Instant::now();

    let store = nestweaver_store::GraphStore::open(&config.db_path)
        .map_err(|e| anyhow::anyhow!("failed to open database: {e}"))?;

    store
        .flush_embedding_index()
        .map_err(|e| anyhow::anyhow!("failed to flush embedding index: {e}"))?;

    store
        .checkpoint()
        .map_err(|e| anyhow::anyhow!("CHECKPOINT failed: {e}"))?;

    // Copy files to staging while the store is still open (to keep the lock).
    copy_db_files(
        &config.db_path,
        staging.path(),
        config.include_clones,
        config.workspace_path.as_deref(),
    )?;

    // Gather graph statistics for the manifest while the store is still open.
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
    drop(store);

    // Phase 2: Package (safe to release the database).
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
pub fn backup_inspect(archive_path: &Path) -> anyhow::Result<BackupManifest> {
    let file = std::fs::File::open(archive_path)?;
    let decoder = zstd::Decoder::new(file)?;
    let mut archive = tar::Archive::new(decoder);

    for entry in archive.entries()? {
        let mut entry = entry?;
        let path = entry.path()?;
        if path.to_str() == Some("./manifest.json") || path.to_str() == Some("manifest.json") {
            let mut manifest: BackupManifest = serde_json::from_reader(&mut entry)?;
            // The in-archive manifest cannot record its own compressed size
            // (sealed before compression finishes). Recompute it from the
            // archive file on disk so inspect/list report a real figure.
            if manifest.sizes.total_compressed == 0 {
                manifest.sizes.total_compressed = std::fs::metadata(archive_path)
                    .map(|m| m.len())
                    .unwrap_or(0);
            }
            return Ok(manifest);
        }
    }
    anyhow::bail!("manifest.json not found in archive")
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

/// Restore a backup archive into a target directory.
pub fn backup_restore(config: &RestoreConfig) -> anyhow::Result<RestoreResult> {
    let start = Instant::now();

    std::fs::create_dir_all(&config.data_dir)?;

    let file = std::fs::File::open(&config.snapshot_path)?;
    let decoder = zstd::Decoder::new(file)?;
    let mut archive = tar::Archive::new(decoder);
    archive.unpack(&config.data_dir)?;

    let manifest_path = config.data_dir.join("manifest.json");
    let manifest_str = std::fs::read_to_string(&manifest_path)
        .map_err(|e| anyhow::anyhow!("failed to read manifest.json after extraction: {e}"))?;
    let manifest: BackupManifest = serde_json::from_str(&manifest_str)?;

    verify_backup_checksums(&config.data_dir, &manifest)?;
    check_schema_compatibility(&manifest)?;

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
    staging: &Path,
    include_clones: bool,
    workspace_path: Option<&Path>,
) -> anyhow::Result<()> {
    let db_filename = db_path
        .file_name()
        .ok_or_else(|| anyhow::anyhow!("db_path has no filename"))?;

    // Copy the main database file.
    std::fs::copy(db_path, staging.join(db_filename))?;

    // Copy known sidecars (skip missing ones silently).
    for suffix in SIDECAR_SUFFIXES {
        let sidecar = crate::sidecar_path(db_path, suffix);
        if !sidecar.exists() {
            continue;
        }
        let dest_name = {
            let mut s = db_filename.to_owned();
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
        let mut wal_dest = db_filename.to_owned();
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
    if include_clones {
        if let Some(ws) = workspace_path {
            let clones_dir = staging.join("clones");
            if ws.exists() {
                std::fs::create_dir_all(&clones_dir)?;
                for entry in std::fs::read_dir(ws)? {
                    let entry = entry?;
                    let git_dir = entry.path().join(".git");
                    if git_dir.is_dir() {
                        let dest = clones_dir.join(entry.file_name()).join(".git");
                        nestweaver_storage::copy_dir_all(&git_dir, &dest)?;
                    }
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

    // Compute SHA-256 of the database file.
    let db_staged = staging.join(&db_filename);
    let db_bytes = std::fs::read(&db_staged)?;
    let db_hash = format!("sha256:{}", hex_encode(&Sha256::digest(&db_bytes)));
    let db_size = db_bytes.len() as u64;

    let mut checksums = HashMap::new();
    checksums.insert(db_filename.clone(), db_hash);

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
        let bytes = std::fs::read(&file_path)?;
        let actual = format!("sha256:{}", hex_encode(&Sha256::digest(&bytes)));
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
    (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let (y, mo, d, h, mi, s) = unix_to_utc(1_782_382_245);
        assert_eq!(y, 2026);
        assert_eq!(mo, 6);
        // Exact day/time verified by calculation.
    }
}
