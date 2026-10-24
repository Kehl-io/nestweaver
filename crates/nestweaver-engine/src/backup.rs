use anyhow::Context;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// The complete set of on-disk artifacts a database directory can hold,
/// documented here because nothing else does (nw-368): not `brain
/// reindex-search --help`, not `INSTALL.md`, not `docs/`. An operator who
/// finds an unfamiliar file beside `<db>.lbug` should be able to find this
/// list rather than guess whether it is safe to touch.
///
/// A fresh index with `--with-trigrams` lays down all thirteen of:
/// `.engine-format`, `.filemeta.json`, `.generation`, `.manifests.json`, `.pagerank.json`,
/// `.parsed_cache.bin`, `.publications/`, `.resolution_deps.bin`,
/// `.resolver_generation.json`, `.tantivy/`, `.wal`, `.write.lock`, and
/// `.regex-v3/`. [`SIDECAR_SUFFIXES`] below is the subset a backup archives —
/// it deliberately excludes three of these:
///
/// * `.wal` — copied separately by [`copy_db_files`] only if it still exists
///   (it should not, after a clean `CHECKPOINT`); never itself a backup
///   member name.
/// * `.write.lock` — the exclusive-writer lease file
///   ([`nestweaver_store::write_lease_path`]). Process-scoped and meaningless
///   copied into an archive; a restored database gets a fresh one on next
///   open.
/// * `.publications/` — the staging area an in-progress publication cutover
///   builds under, not restorable state; a live publication is backed up via
///   its own generation, not this directory.
///
/// Two further undocumented-until-now artifacts are NOT sidecars of a healthy
/// index at all, but residue a crashed schema migration can leave beside one
/// (nw-368's other half):
///
/// * `<tantivy-dir>.reindexing` — the previous Tantivy index, renamed aside
///   during a migration so it can be rolled back to. See
///   [`nestweaver_store::reindex_lock_path`] for the lock that guards it and
///   `recover_interrupted_reindex` (private to `nestweaver-store`) for the
///   recovery this drives on the next open.
/// * `<parent>/`[`nestweaver_store::TANTIVY_REINDEX_STAGING_PREFIX`]`*` — the
///   replacement index a migration builds before installing it. Unlike the
///   `.reindexing` backup, ordinary recovery does not reclaim this on its
///   own: a migration killed before either rename leaves it behind with
///   nothing to signal it needs cleanup. Call
///   [`nestweaver_store::reclaim_orphaned_tantivy_staging`] to find and
///   remove orphaned instances of this prefix.
///
/// Known sidecar suffixes to include in backups.
///
/// `.vault-registrations.json` (nw-587) is deliberately NOT listed: it is
/// evidence about which vaults the LIVE database has published, used to
/// disclose vaults a WAL move-aside dropped. Restoring an older copy of it
/// would make a restored database claim registrations it never had, and after
/// a restore any vault added since the backup is correctly reported missing
/// from the current file.
const SIDECAR_SUFFIXES: &[&str] = &[
    ".tantivy",
    ".regex-v3",
    ".pagerank.json",
    ".parsed_cache.bin",
    // Parses added since the base was last written; read with it.
    ".parsed_cache.log",
    ".resolution_deps.bin",
    crate::resolver_generation::RESOLVER_GENERATION_SIDECAR,
    // nw-670: the link-rules version travels with the links it describes, so
    // a restored pre-upgrade graph is migrated rather than trusted.
    crate::code_links::CODE_LINKS_SIDECAR,
    // Owed cross-repo link inference travels with the links it describes.
    crate::cross_repo_links::CROSS_REPO_LINKS_SIDECAR,
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
    // The storage-engine format the graph file was built with. It travels with
    // the graph so a restored database opens without re-deriving it.
    nestweaver_store::engine_format::ENGINE_FORMAT_SIDECAR_SUFFIX,
];

/// Current backup manifest version. Version 2 embeds the same typed
/// `PublicationBundleV3` inventory used by snapshots and publication cutover.
const MANIFEST_VERSION: u32 = 2;

/// Hard ceiling for the in-archive `publication.json`, which is the only member
/// read into memory rather than streamed. It is a JSON inventory of artifact
/// descriptors — a few hundred KB even for a large publication — so 64 MiB is
/// generous while still bounding a hostile or corrupt header (nw-144).
const MAX_PUBLICATION_MANIFEST_BYTES: u64 = 64 * 1024 * 1024;

/// An expected refusal of a destructive or conflicting backup invocation.
/// Kept typed across CLI and daemon boundaries so callers can report usage
/// errors without classifying corruption or I/O failures as user mistakes.
#[derive(Debug)]
pub struct BackupRefusal(pub String);
impl std::fmt::Display for BackupRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for BackupRefusal {}

macro_rules! refuse_backup {
    ($($arg:tt)*) => { return Err(BackupRefusal(format!($($arg)*)).into()) };
}

macro_rules! require_backup {
    ($condition:expr, $($arg:tt)*) => {
        if !$condition { refuse_backup!($($arg)*); }
    };
}

fn restore_format_error(error: impl std::error::Error + Send + Sync + 'static) -> anyhow::Error {
    anyhow::Error::new(error).context(BackupRefusal(
        "backup restore refused: invalid recovery metadata; all copies preserved".into(),
    ))
}

fn expected_directory_error(error: anyhow::Error, path: &Path) -> anyhow::Error {
    if error.downcast_ref::<std::io::Error>().is_some_and(|error| {
        matches!(
            error.kind(),
            std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
        ) || error.raw_os_error() == Some(libc::ELOOP)
    }) {
        error.context(BackupRefusal(format!(
            "backup restore refused: recorded directory is missing or replaced: {}",
            path.display()
        )))
    } else {
        error
    }
}

#[derive(Debug, Clone)]
pub struct BackupConfig {
    pub db_path: PathBuf,
    pub output_path: PathBuf,
    pub include_clones: bool,
    pub instance_id: String,
    pub workspace_path: Option<PathBuf>,
    /// Replace an archive that already exists at `output_path`. Without this,
    /// save leaves that file untouched.
    pub overwrite: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackupManifest {
    /// Rebuildable data deliberately omitted from this archive.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
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
    /// Replace a recognized non-empty `data_dir`. Proven archived layouts
    /// can be retired; live layouts are retained outside the active slots.
    /// Without this flag an occupied target refuses replacement.
    pub replace: bool,
}

#[derive(Debug)]
pub struct RestoreResult {
    pub manifest: BackupManifest,
    pub data_dir: PathBuf,
    pub duration: Duration,
    /// A pre-restore copy left by an EARLIER interrupted restore whose
    /// redundancy this run could not prove, and which was therefore renamed
    /// aside instead of deleted. Surfaced so the operator can remove it once
    /// satisfied — silently keeping it would leak a full copy of the graph.
    pub preserved_copy: Option<PathBuf>,
    /// Every retained dataset, including a superseded live incumbent. The
    /// compatibility field above aliases the first path.
    pub preserved_copies: Vec<PathBuf>,
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
    seal_publication_slot_inner(db_path, slot_root, false)
}

/// Seal a live staged slot while retaining its exact database writer authority.
/// The operational lock stays in place; it is never an archived artifact.
pub fn seal_publication_slot_with_authority(
    db_path: &Path,
    slot_root: &Path,
    authority: &nestweaver_store::DbWriteLease,
) -> anyhow::Result<crate::publication::PublicationBundleV3> {
    if !authority.authorizes(db_path) {
        anyhow::bail!("publication seal requires authority for the exact staged database");
    }
    seal_publication_slot_inner(db_path, slot_root, true)
}

fn seal_publication_slot_inner(
    db_path: &Path,
    slot_root: &Path,
    live_writer: bool,
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
    let store = nestweaver_store::GraphStore::open_read_only_without_migration(db_path)
        .map_err(|error| anyhow::anyhow!("open staged publication graph: {error}"))?;
    let identity = store
        .publication_identity()
        .map_err(|error| anyhow::anyhow!("read staged publication identity: {error}"))?
        .ok_or_else(|| anyhow::anyhow!("staged publication graph has no identity"))?;
    let source_graph_generation = store.graph_generation();

    let config = BackupConfig {
        db_path: db_path.to_path_buf(),
        output_path: slot_root.join("unused.nwsnap.zst"),
        include_clones: false,
        instance_id: "publication".to_string(),
        workspace_path: None,
        overwrite: false,
    };
    let (bundle, _excluded) = build_backup_publication_bundle_inner(
        &config,
        slot_root,
        &identity,
        source_graph_generation,
        live_writer,
    )?;
    crate::publication::validate_slot_contents(
        slot_root,
        &bundle,
        crate::publication::ArtifactBytes::Verified,
    )
    .map_err(|error| anyhow::anyhow!("validate staged publication: {error}"))?;
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
    logical_instance_id: String,
    /// The graph was built by a storage engine older than LadybugDB 0.21 and
    /// was read through the store's legacy read-only exemption. The archive
    /// is sealed at the pre-cutover bundle format so a restore warns.
    pre_cutover_engine: bool,
}

/// Resolve portable backup identity from graph ownership, never a runtime path hash.
pub fn backup_logical_instance_id(store: &nestweaver_store::GraphStore) -> anyhow::Result<String> {
    if let Some(identity) = store.data_instance_id()? {
        if identity.trim().is_empty() {
            anyhow::bail!("database-owned logical instance identity is empty");
        }
        return Ok(identity);
    }
    let observed = store.observed_instance_ids()?;
    match observed.as_slice() {
        [] => Ok("default".to_string()),
        [identity] => Ok(identity.clone()),
        _ => anyhow::bail!(
            "cannot infer backup logical instance identity from multiple graph instances"
        ),
    }
}

/// Stage a backup from an ALREADY-OPEN store: flush embeddings, `CHECKPOINT` the
/// WAL into the main file, copy the on-disk files to a temp staging dir, and
/// gather manifest stats. Reuses the caller's live database handle, draining
/// existing connections before opening the checkpoint connection. It also owns
/// the store's publication lease through checkpoint,
/// copy, and statistics collection, so UI watchers and other publishers cannot
/// mutate the graph mid-copy. The configured path must resolve to this store;
/// all database and sidecar reads derive from `store.db_path()`. An inherited
/// dirty publication is rejected.
/// The returned [`StagedBackup`] is packaged lock-free by [`package_staged`].
pub fn stage_backup_from_store(
    store: &nestweaver_store::GraphStore,
    config: &BackupConfig,
) -> anyhow::Result<StagedBackup> {
    stage_backup_with_statistics(store, config, &StoreBackupStatistics)
}

trait BackupStatistics {
    fn count(&self, store: &nestweaver_store::GraphStore) -> anyhow::Result<usize>;
    fn per_repo(
        &self,
        store: &nestweaver_store::GraphStore,
    ) -> anyhow::Result<HashMap<String, usize>>;
    fn repos(
        &self,
        store: &nestweaver_store::GraphStore,
    ) -> anyhow::Result<Vec<nestweaver_schema::Repo>>;
}
struct StoreBackupStatistics;
impl BackupStatistics for StoreBackupStatistics {
    fn count(&self, store: &nestweaver_store::GraphStore) -> anyhow::Result<usize> {
        Ok(store.count_symbols()?)
    }
    fn per_repo(
        &self,
        store: &nestweaver_store::GraphStore,
    ) -> anyhow::Result<HashMap<String, usize>> {
        Ok(store.count_symbols_by_repo()?)
    }
    fn repos(
        &self,
        store: &nestweaver_store::GraphStore,
    ) -> anyhow::Result<Vec<nestweaver_schema::Repo>> {
        Ok(store.list_repos(None)?)
    }
}
fn stage_backup_with_statistics(
    store: &nestweaver_store::GraphStore,
    config: &BackupConfig,
    statistics: &dyn BackupStatistics,
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
    refuse_output_in_database_footprint(&config.db_path, &config.output_path)?;
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

    // A pre-cutover graph is read, never written: no compaction and no
    // CHECKPOINT (a 0.21 checkpoint would rewrite its pages with the new
    // hash). Its log, if any, is copied as it is.
    let pre_cutover_engine = store.is_legacy_engine();
    if !pre_cutover_engine {
        store
            .compact_embedding_index()
            .map_err(|e| anyhow::anyhow!("failed to compact embedding index: {e}"))?;
        store
            .checkpoint_waiting(Duration::from_secs(5))
            .map_err(|e| anyhow::anyhow!("CHECKPOINT failed: {e}"))?;
    }

    // Copy files while the caller holds the write lock (sidecars are non-atomic).
    copy_db_files(
        store_db_path,
        staged_db_filename,
        staging.path(),
        config.include_clones,
        config.workspace_path.as_deref(),
    )?;

    // Gather graph statistics for the manifest while the store is open.
    let symbol_count = statistics
        .count(store)
        .context("read backup symbol count")?;
    let per_repo = statistics
        .per_repo(store)
        .context("read backup per-repository counts")?;
    let repos: Vec<BackupRepoInfo> = statistics
        .repos(store)
        .context("read backup repository inventory")?
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
    let logical_instance_id = backup_logical_instance_id(store)?;

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
        logical_instance_id,
        pre_cutover_engine,
    })
}

pub fn backup_save(config: &BackupConfig) -> anyhow::Result<BackupResult> {
    let authority = nestweaver_store::acquire_db_write_lease(&config.db_path).map_err(|error| {
        anyhow::anyhow!(
            "failed to acquire writer authority for backup of {}: {error:?}",
            config.db_path.display()
        )
    })?;
    // "Back up before upgrading" must work after the upgrade too: a database
    // an older storage engine built is read through the store's legacy
    // read-only exemption (scans only, no writes), under the same authority.
    let store =
        match nestweaver_store::GraphStore::open_with_authority(&config.db_path, &authority) {
            Err(nestweaver_store::StoreError::RebuildRequired { .. }) => {
                nestweaver_store::GraphStore::open_legacy_engine_read_only(&config.db_path)
            }
            other => other,
        }
        .map_err(|e| anyhow::anyhow!("failed to open database: {e}"))?;
    let staged = stage_backup_from_store(&store, config)?;
    drop(store);
    drop(authority);
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
        logical_instance_id,
        pre_cutover_engine,
    } = staged;
    let activity_path = staging.path().join(
        config
            .db_path
            .file_name()
            .ok_or_else(|| anyhow::anyhow!("database filename is missing"))?,
    );
    let activity_path = crate::sidecar_path(&activity_path, ".gitactivity.json");
    let mut warnings = Vec::new();
    if activity_path.try_exists()? && legacy_git_activity_payload(&std::fs::read(&activity_path)?)?
    {
        std::fs::remove_file(&activity_path)
            .context("exclude legacy activity from backup staging")?;
        warnings.push("Legacy unversioned git-activity scores were excluded from this backup because repository ownership cannot be recovered safely. Graph data is preserved; reindex repositories with --with-git-activity after restore to rebuild activity ranking.".to_string());
    }
    let (mut bundle, excluded) = build_backup_publication_bundle(
        config,
        staging.path(),
        &publication_identity,
        source_graph_generation,
    )?;
    for item in &excluded {
        warnings.push(format!("excluded rebuildable artifact: {item}"));
    }
    if pre_cutover_engine {
        // Seal a pre-cutover graph at the pre-cutover format: a reader must
        // never mistake it for a graph this engine can open.
        bundle.format_version = crate::snapshot::LEGACY_ENGINE_SNAPSHOT_FORMAT_VERSION;
        bundle.validate_metadata(crate::snapshot::LEGACY_ENGINE_SNAPSHOT_FORMAT_VERSION)?;
    }
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
    manifest.instance_id = logical_instance_id;
    if pre_cutover_engine {
        warnings.push(
            "this archive holds a database built by a storage engine older than LadybugDB \
             0.21, backed up unchanged (read-only, no checkpoint). Keep it as the rollback \
             copy; the database itself must be rebuilt before this version opens it: stop \
             the daemon and run `nestweaver publication rebuild --config <instance.toml>`"
                .to_string(),
        );
    }
    manifest.warnings = warnings;
    let manifest_json = serde_json::to_string_pretty(&manifest)?;
    std::fs::write(staging.path().join("manifest.json"), &manifest_json)?;

    // Re-checked at publish time: the destination may have changed since
    // staging began (a symlink or hard link made in the meantime).
    refuse_output_in_database_footprint(&config.db_path, &config.output_path)?;
    publish_backup_archive(staging.path(), &config.output_path, config.overwrite)?;

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

// ---------------------------------------------------------------------------
// Durable restore journal
// ---------------------------------------------------------------------------

/// How far a restore had got when it stopped.
///
/// This type exists because "the destination directory is not empty" is not,
/// and never was, evidence that a restore finished. The cutover falls back to a
/// recursive copy whenever the atomic rename cannot be used, and a copy that
/// dies after its first file leaves a destination that is NONEMPTY AND USELESS
/// while the only complete copy of the user's data sits in `<data>.restoring`.
/// The previous recovery read nonemptiness as completion and deleted that copy
/// — the last complete copy — before the retry.
///
/// Every phase below is published DURABLY (see [`publish_restore_phase`]) and
/// is ordered against the filesystem effect it describes so that the journal
/// can never claim more than the disk has reached:
///
/// * phases that say "the destination is not to be trusted" are published
///   BEFORE the action, because over-claiming them is safe;
/// * phases that say "the replacement is complete" are published AFTER the
///   action, its `fsync`, and its validation, because over-claiming those is
///   exactly the data loss.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RestorePhase {
    /// The archive is extracted into a staging directory and has passed every
    /// integrity check. Nothing in the target has been touched yet.
    StagingVerified,
    /// The incumbent data directory has been renamed to `<data>.restoring`,
    /// which now holds the only complete copy of the user's pre-restore data.
    OldDataPreserved,
    /// The destination is being built and is NOT authoritative, however many
    /// files it already contains. Published before the first byte of a
    /// cross-device copy lands, and before a rollback after a failed
    /// post-cutover validation.
    CopyInProgress,
    /// The replacement is fully in place, durable on disk, and validated. Only
    /// now is `<data>.restoring` provably redundant.
    CutoverValidated,
    /// The preserved copy is being removed.
    Cleanup,
}

/// Bumped when the on-disk journal shape changes. A journal this build cannot
/// read cannot authorize mutation. Legacy v2/v3 retain their explicit conservative
/// recovery behavior; v4 requires a domain-separated disposition.
const RESTORE_JOURNAL_VERSION: u32 = 4;

/// Metadata-only origin witness: stat does not open/close a leased database
/// descriptor (which would release this process's POSIX record locks).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct RestoreFileWitness {
    device: u64,
    inode: u64,
    length: u64,
    modified_seconds: i64,
    modified_nanoseconds: i64,
    changed_seconds: i64,
    changed_nanoseconds: i64,
}

fn restore_file_witness(path: &Path) -> anyhow::Result<RestoreFileWitness> {
    let metadata = std::fs::symlink_metadata(path)?;
    anyhow::ensure!(
        metadata.is_file(),
        "restore file witness requires a regular file: {}",
        path.display()
    );
    restore_file_witness_from_metadata(&metadata)
}

fn restore_file_witness_from_metadata(
    metadata: &std::fs::Metadata,
) -> anyhow::Result<RestoreFileWitness> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        Ok(RestoreFileWitness {
            device: metadata.dev(),
            inode: metadata.ino(),
            length: metadata.len(),
            modified_seconds: metadata.mtime(),
            modified_nanoseconds: metadata.mtime_nsec(),
            changed_seconds: metadata.ctime(),
            changed_nanoseconds: metadata.ctime_nsec(),
        })
    }
    #[cfg(not(unix))]
    {
        anyhow::bail!("physical restore file witnesses are unsupported on this platform")
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct OriginalLiveProof {
    identity: (u64, u64),
    paths: Vec<String>,
    files: HashMap<String, RestoreFileWitness>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct LivePreservation {
    original: OriginalLiveProof,
    namespace: PathBuf,
    namespace_identity: (u64, u64),
    reservation: PathBuf,
    reservation_identity: (u64, u64),
    preserved_path: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "policy", deny_unknown_fields)]
enum RestoreDisposition {
    DeleteProvenArchive,
    PreserveLive { receipt: Box<LivePreservation> },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RestoreJournal {
    version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    disposition: Option<RestoreDisposition>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    previous_preserved_copies: Vec<PathBuf>,
    phase: RestorePhase,
    data_dir: PathBuf,
    restoring_dir: PathBuf,
    snapshot_path: PathBuf,
    #[serde(default)]
    destination_identity: Option<(u64, u64)>,
    #[serde(default)]
    restoring_identity: Option<(u64, u64)>,
    #[serde(default)]
    destination_paths: Vec<String>,
    #[serde(default)]
    restoring_paths: Vec<String>,
    // Content authority captured from the validated archive BEFORE cutover.
    // Older journals remain readable but cannot authorize marker synthesis.
    #[serde(default)]
    graph_path: Option<String>,
    #[serde(default)]
    source_checksums: Option<HashMap<String, String>>,
    #[serde(default)]
    restoring_file_witnesses: Option<HashMap<String, RestoreFileWitness>>,
    #[serde(default)]
    destination_file_witnesses: Option<HashMap<String, RestoreFileWitness>>,
    #[serde(default)]
    legacy_engine: bool,
}

impl OriginalLiveProof {
    fn capture(path: &Path) -> anyhow::Result<Self> {
        let directory = AnchoredDirectory::open(path)?;
        let (paths, files) = directory.snapshot()?;
        Ok(Self {
            identity: directory.identity()?,
            paths,
            files,
        })
    }

    fn verify(&self, path: &Path) -> anyhow::Result<()> {
        let observed =
            Self::capture(path).map_err(|error| expected_directory_error(error, path))?;
        require_backup!(
            observed.identity == self.identity
                && observed
                    .paths
                    .iter()
                    .collect::<std::collections::BTreeSet<_>>()
                    == self.paths.iter().collect::<std::collections::BTreeSet<_>>()
                && observed.files == self.files,
            "original live content proof changed; all copies and journal preserved"
        );
        Ok(())
    }

    fn proves(&self, path: &Path) -> bool {
        Self::capture(path).is_ok_and(|proof| {
            proof.identity == self.identity
                && proof
                    .paths
                    .iter()
                    .collect::<std::collections::BTreeSet<_>>()
                    == self.paths.iter().collect::<std::collections::BTreeSet<_>>()
                && proof.files == self.files
        })
    }
}

impl LivePreservation {
    fn reserve(data_dir: &Path) -> anyhow::Result<Self> {
        let namespace = std::fs::canonicalize(
            data_dir
                .parent()
                .filter(|parent| !parent.as_os_str().is_empty())
                .unwrap_or_else(|| Path::new(".")),
        )?;
        let namespace_identity = restore_directory_identity(&namespace)
            .ok_or_else(|| anyhow::anyhow!("missing restore namespace"))?;
        let source = AnchoredDirectory::open(&namespace)?;
        let name = format!(
            "{}.preserved-{}",
            data_dir
                .file_name()
                .ok_or_else(|| anyhow::anyhow!("restore target has no name"))?
                .to_string_lossy(),
            uuid::Uuid::new_v4()
        );
        maybe_fail_live_restore_io("mkdir")?;
        source.mkdir(&name).with_context(|| {
            format!(
                "reserve live retention directory {}",
                namespace.join(&name).display()
            )
        })?;
        let reservation = namespace.join(&name);
        let directory = source.child_directory(std::ffi::OsStr::new(&name))?;
        directory.file.sync_all()?;
        source.file.sync_all()?;
        Ok(Self {
            original: OriginalLiveProof::capture(data_dir).with_context(|| {
                format!(
                    "empty live retention reservation remains at {}",
                    reservation.display()
                )
            })?,
            namespace,
            namespace_identity,
            reservation_identity: directory.identity()?,
            preserved_path: reservation.join("original"),
            reservation,
        })
    }

    fn validate_paths(&self, data_dir: &Path) -> anyhow::Result<()> {
        let candidate = data_dir
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        let parent = std::fs::canonicalize(candidate)
            .map_err(|error| expected_directory_error(error.into(), candidate))?;
        require_backup!(
            self.namespace == parent
                && self.reservation.parent() == Some(parent.as_path())
                && self.preserved_path == self.reservation.join("original")
                && self.reservation != absolute_lexically(data_dir)
                && self.reservation != absolute_lexically(&restoring_dir_for(data_dir))
                && self.reservation.file_name().is_some_and(|name| name
                    .to_string_lossy()
                    .starts_with(&format!(
                        "{}.preserved-",
                        data_dir.file_name().unwrap_or_default().to_string_lossy()
                    ))),
            "invalid live preservation placement; all copies preserved"
        );
        Ok(())
    }

    /// The directory FDs anchor both sides of the rename. No graph descriptor
    /// is opened: even a transient close could release POSIX database locks.
    fn finish(&self, data_dir: &Path) -> anyhow::Result<PathBuf> {
        self.finish_with_hook(data_dir, |_| Ok(()))
    }

    fn finish_with_hook(
        &self,
        data_dir: &Path,
        mut hook: impl FnMut(&str) -> anyhow::Result<()>,
    ) -> anyhow::Result<PathBuf> {
        self.validate_paths(data_dir)?;
        let source = AnchoredDirectory::open(&self.namespace)
            .map_err(|error| expected_directory_error(error, &self.namespace))?;
        let destination = AnchoredDirectory::open(&self.reservation)
            .map_err(|error| expected_directory_error(error, &self.reservation))?;
        self.check_bindings(&source, &destination)?;
        let aside = restoring_dir_for(data_dir);
        let source_name = aside
            .file_name()
            .ok_or_else(|| BackupRefusal("missing aside name in recovery proof".into()))?;
        let source_entry = source.path().join(source_name);
        let destination_entry = destination.path().join("original");
        if source_entry.try_exists()? {
            self.original.verify(&source_entry)?;
            require_backup!(
                std::fs::read_dir(destination.path())?.next().is_none(),
                "preservation reservation is not empty; no path was replaced"
            );
            hook("before-rename")?;
            self.check_bindings(&source, &destination)?;
            maybe_crash_restore_step_for_test("before-live-preserve");
            maybe_fail_live_restore_io("rename")?;
            source.rename_noreplace(source_name, &destination, std::ffi::OsStr::new("original"))?;
            hook("after-rename")?;
            maybe_crash_restore_step_for_test("after-live-preserve");
        }
        let reserved_entries =
            std::fs::read_dir(destination.path())?.collect::<Result<Vec<_>, _>>()?;
        require_backup!(
            reserved_entries.len() == 1 && reserved_entries[0].file_name() == "original",
            "preservation reservation contains unknown entries; all copies retained"
        );
        require_backup!(
            !source_entry.try_exists()?,
            "both original and retained live copies exist; journal preserved"
        );
        self.original.verify(&destination_entry)?;
        self.check_bindings(&source, &destination)?;
        maybe_fail_live_restore_io("reservation-fsync")?;
        destination.file.sync_all().with_context(|| {
            format!(
                "live copy preserved at {}; reservation fsync failed; journal retained",
                self.preserved_path.display()
            )
        })?;
        maybe_fail_live_restore_io("parent-fsync")?;
        source.file.sync_all().with_context(|| {
            format!(
                "live copy preserved at {}; parent fsync failed; journal retained",
                self.preserved_path.display()
            )
        })?;
        // Recheck after durability and before the caller retires the journal.
        self.check_bindings(&source, &destination)?;
        self.original.verify(&destination_entry)?;
        maybe_crash_restore_step_for_test("live-preserve-durable");
        Ok(self.preserved_path.clone())
    }

    /// An unvalidated rollback never used its reserved child. Remove only the
    /// exact empty reservation, with atomic rmdir semantics (no recursive IO).
    fn retire_empty_reservation(&self, data_dir: &Path) -> anyhow::Result<()> {
        self.validate_paths(data_dir)?;
        let parent = AnchoredDirectory::open(&self.namespace)?;
        if std::fs::symlink_metadata(&self.reservation)
            .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound)
        {
            require_backup!(
                parent.identity()? == self.namespace_identity
                    && restore_directory_identity(&self.namespace) == Some(self.namespace_identity)
                    && self.original.proves(data_dir),
                "missing reservation with no complete reinstated original; journal retained"
            );
            return Ok(());
        }
        let reservation = AnchoredDirectory::open(&self.reservation)?;
        self.check_bindings(&parent, &reservation)?;
        require_backup!(
            std::fs::read_dir(reservation.path())?.next().is_none(),
            "unused live reservation has unknown contents; retained at {}",
            self.reservation.display()
        );
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            use std::os::unix::ffi::OsStrExt;
            let name = std::ffi::CString::new(
                self.reservation
                    .file_name()
                    .ok_or_else(|| anyhow::anyhow!("missing reservation name"))?
                    .as_bytes(),
            )?;
            // SAFETY: held parent FD; AT_REMOVEDIR cannot delete a nonempty
            // reservation or follow a replacement symlink.
            if unsafe { libc::unlinkat(parent.file.as_raw_fd(), name.as_ptr(), libc::AT_REMOVEDIR) }
                != 0
            {
                return Err(std::io::Error::last_os_error()).with_context(|| {
                    format!(
                        "unused reservation retained at {}",
                        self.reservation.display()
                    )
                });
            }
            parent.file.sync_all()?;
        }
        #[cfg(not(unix))]
        anyhow::bail!("descriptor anchored restore is unsupported");
        Ok(())
    }

    fn check_bindings(
        &self,
        source: &AnchoredDirectory,
        destination: &AnchoredDirectory,
    ) -> anyhow::Result<()> {
        require_backup!(
            source.identity()? == self.namespace_identity
                && destination.identity()? == self.reservation_identity
                && restore_directory_identity(&self.namespace) == Some(self.namespace_identity)
                && restore_directory_identity(&self.reservation) == Some(self.reservation_identity),
            "live preservation namespace changed; journal retained, last known copy path {}; observed reservation {}",
            self.preserved_path.display(),
            destination
                .observed_path()
                .unwrap_or_else(|| self.reservation.clone())
                .display()
        );
        Ok(())
    }
}

struct AnchoredDirectory {
    file: std::fs::File,
}

impl AnchoredDirectory {
    fn open(path: &Path) -> anyhow::Result<Self> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            Ok(Self {
                file: std::fs::OpenOptions::new()
                    .read(true)
                    .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
                    .open(path)?,
            })
        }
        #[cfg(not(unix))]
        {
            let _ = path;
            anyhow::bail!("descriptor anchored restore is unsupported")
        }
    }

    fn identity(&self) -> anyhow::Result<(u64, u64)> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let metadata = self.file.metadata()?;
            Ok((metadata.dev(), metadata.ino()))
        }
        #[cfg(not(unix))]
        {
            anyhow::bail!("descriptor anchored restore is unsupported")
        }
    }

    fn path(&self) -> PathBuf {
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            #[cfg(target_os = "linux")]
            {
                PathBuf::from(format!("/proc/self/fd/{}", self.file.as_raw_fd()))
            }
            #[cfg(not(target_os = "linux"))]
            {
                PathBuf::from(format!("/dev/fd/{}", self.file.as_raw_fd()))
            }
        }
        #[cfg(not(unix))]
        {
            PathBuf::new()
        }
    }

    fn observed_path(&self) -> Option<PathBuf> {
        #[cfg(target_os = "macos")]
        {
            use std::os::fd::AsRawFd;
            use std::os::unix::ffi::OsStringExt;
            let mut path = vec![0u8; libc::PATH_MAX as usize];
            // SAFETY: F_GETPATH writes at most PATH_MAX bytes to this buffer.
            if unsafe { libc::fcntl(self.file.as_raw_fd(), libc::F_GETPATH, path.as_mut_ptr()) }
                != 0
            {
                return None;
            }
            let end = path.iter().position(|byte| *byte == 0)?;
            path.truncate(end);
            Some(PathBuf::from(std::ffi::OsString::from_vec(path)))
        }
        #[cfg(not(target_os = "macos"))]
        {
            std::fs::read_link(self.path()).ok()
        }
    }

    fn snapshot(&self) -> anyhow::Result<(Vec<String>, HashMap<String, RestoreFileWitness>)> {
        fn walk(
            directory: &AnchoredDirectory,
            prefix: &str,
            paths: &mut Vec<String>,
            files: &mut HashMap<String, RestoreFileWitness>,
        ) -> anyhow::Result<()> {
            for entry in std::fs::read_dir(directory.path())? {
                let entry = entry?;
                let name = entry.file_name();
                let name_text = name
                    .to_str()
                    .ok_or_else(|| BackupRefusal("non-UTF8 preserved path".into()))?;
                let relative = format!("{prefix}{name_text}");
                require_backup!(
                    paths.len() < 100_000
                        && relative.len() < 4096
                        && prefix.bytes().filter(|byte| *byte == b'/').count() < 64,
                    "live preservation inventory exceeds its bound"
                );
                let metadata = std::fs::symlink_metadata(directory.path().join(&name))?;
                paths.push(relative.clone());
                if metadata.is_dir() {
                    let child = directory.child_directory(&name)?;
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::MetadataExt;
                        require_backup!(
                            child.identity()? == (metadata.dev(), metadata.ino()),
                            "preservation child changed"
                        );
                    }
                    walk(&child, &format!("{relative}/"), paths, files)?;
                } else {
                    require_backup!(
                        regular_unique_file(&metadata),
                        "non-regular or linked preserved entry"
                    );
                    files.insert(relative, restore_file_witness_from_metadata(&metadata)?);
                }
            }
            Ok(())
        }
        let mut paths = Vec::new();
        let mut files = HashMap::new();
        walk(self, "", &mut paths, &mut files)?;
        Ok((paths, files))
    }

    fn child_directory(&self, name: &std::ffi::OsStr) -> anyhow::Result<Self> {
        #[cfg(unix)]
        {
            use std::os::fd::{AsRawFd, FromRawFd};
            use std::os::unix::ffi::OsStrExt;
            let name = std::ffi::CString::new(name.as_bytes())?;
            // SAFETY: child is opened relative to the held directory, without
            // following a symlink; ownership of a successful FD is transferred.
            let fd = unsafe {
                libc::openat(
                    self.file.as_raw_fd(),
                    name.as_ptr(),
                    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                )
            };
            if fd < 0 {
                return Err(std::io::Error::last_os_error().into());
            }
            Ok(Self {
                file: unsafe { std::fs::File::from_raw_fd(fd) },
            })
        }
        #[cfg(not(unix))]
        {
            let _ = name;
            anyhow::bail!("descriptor anchored restore is unsupported")
        }
    }

    fn mkdir(&self, name: &str) -> anyhow::Result<()> {
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            let name = std::ffi::CString::new(name)?;
            // SAFETY: held directory FD and NUL-terminated direct child name.
            if unsafe { libc::mkdirat(self.file.as_raw_fd(), name.as_ptr(), 0o700) } != 0 {
                let error = std::io::Error::last_os_error();
                if error.kind() == std::io::ErrorKind::AlreadyExists {
                    return Err(anyhow::Error::new(error).context(BackupRefusal(
                        "live preservation reservation already exists; nothing was overwritten"
                            .into(),
                    )));
                }
                return Err(error.into());
            }
            Ok(())
        }
        #[cfg(not(unix))]
        {
            let _ = name;
            anyhow::bail!("descriptor anchored restore is unsupported")
        }
    }

    fn rename_noreplace(
        &self,
        name: &std::ffi::OsStr,
        destination: &Self,
        child: &std::ffi::OsStr,
    ) -> anyhow::Result<()> {
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        {
            use std::os::fd::AsRawFd;
            use std::os::unix::ffi::OsStrExt;
            let name = std::ffi::CString::new(name.as_bytes())?;
            let child = std::ffi::CString::new(child.as_bytes())?;
            // SAFETY: held directory descriptors, direct child names, and an
            // atomic exclusive rename. Existing children are never replaced.
            #[cfg(target_os = "linux")]
            let result = unsafe {
                libc::syscall(
                    libc::SYS_renameat2,
                    self.file.as_raw_fd(),
                    name.as_ptr(),
                    destination.file.as_raw_fd(),
                    child.as_ptr(),
                    libc::RENAME_NOREPLACE,
                )
            };
            #[cfg(target_os = "macos")]
            let result = unsafe {
                libc::renameatx_np(
                    self.file.as_raw_fd(),
                    name.as_ptr(),
                    destination.file.as_raw_fd(),
                    child.as_ptr(),
                    libc::RENAME_EXCL,
                )
            };
            if result != 0 {
                let error = std::io::Error::last_os_error();
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::AlreadyExists | std::io::ErrorKind::DirectoryNotEmpty
                ) {
                    return Err(anyhow::Error::new(error).context(BackupRefusal(
                        "live preservation child appeared; nothing was overwritten".into(),
                    )));
                }
                return Err(error.into());
            }
            Ok(())
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        {
            let _ = (name, destination, child);
            anyhow::bail!("atomic no-replace restore is unsupported")
        }
    }
}

fn restore_directory_identity(path: &Path) -> Option<(u64, u64)> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let metadata = std::fs::symlink_metadata(path).ok()?;
        metadata
            .is_dir()
            .then_some((metadata.dev(), metadata.ino()))
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        None
    }
}

fn restore_tree_paths(path: &Path) -> anyhow::Result<Vec<String>> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    walkdir::WalkDir::new(path)
        .min_depth(1)
        .into_iter()
        .map(|entry| {
            let entry = entry?;
            anyhow::ensure!(
                !entry.file_type().is_symlink(),
                "restore ownership is ambiguous at {}",
                entry.path().display()
            );
            normalized_relative_path(path, entry.path())
        })
        .collect()
}

fn journal_proves_owned_tree(path: &Path, identity: Option<(u64, u64)>, paths: &[String]) -> bool {
    identity.is_some()
        && restore_directory_identity(path) == identity
        && restore_tree_paths(path)
            .is_ok_and(|present| present.iter().all(|path| paths.contains(path)))
}

/// Rollback needs the entire original aside, not merely an owned subset.
/// Partial destinations/cleanup still use the subset proof above.
fn journal_proves_complete_owned_tree(
    path: &Path,
    identity: Option<(u64, u64)>,
    paths: &[String],
) -> bool {
    identity.is_some()
        && restore_directory_identity(path) == identity
        && restore_tree_paths(path).is_ok_and(|present| {
            present.iter().collect::<std::collections::BTreeSet<_>>()
                == paths.iter().collect::<std::collections::BTreeSet<_>>()
        })
}

/// Capture the leased incumbent before any destructive cutover. Directory
/// inode/path proofs alone cannot distinguish later same-name replacements.
fn restore_tree_witnesses(path: &Path) -> anyhow::Result<HashMap<String, RestoreFileWitness>> {
    let mut witnesses = HashMap::new();
    if !path.exists() {
        return Ok(witnesses);
    }
    for entry in walkdir::WalkDir::new(path).min_depth(1) {
        let entry = entry?;
        anyhow::ensure!(
            entry.file_type().is_file() || entry.file_type().is_dir(),
            "cannot establish restore content ownership at {}",
            entry.path().display()
        );
        if entry.file_type().is_file() {
            witnesses.insert(
                normalized_relative_path(path, entry.path())?,
                restore_file_witness(entry.path())?,
            );
        }
    }
    Ok(witnesses)
}

/// Rollback needs a complete incumbent; interrupted cleanup may have removed
/// entries already, but every survivor must still have its original witness.
fn journal_proves_original_aside(path: &Path, journal: &RestoreJournal, complete: bool) -> bool {
    if let Some(RestoreDisposition::PreserveLive { receipt }) = &journal.disposition {
        return journal.version == 4 && receipt.original.proves(path);
    }
    let Some(expected) = journal.restoring_file_witnesses.as_ref() else {
        return false;
    };
    let tree_owned = if complete {
        journal_proves_complete_owned_tree(
            path,
            journal.restoring_identity,
            &journal.restoring_paths,
        )
    } else {
        journal_proves_owned_tree(path, journal.restoring_identity, &journal.restoring_paths)
    };
    tree_owned
        && restore_tree_witnesses(path).is_ok_and(|present| {
            (!complete || present.len() == expected.len())
                && present
                    .iter()
                    .all(|(path, hash)| expected.get(path) == Some(hash))
        })
}

/// Where the incumbent data directory is moved to while the replacement lands.
fn restoring_dir_for(data_dir: &Path) -> PathBuf {
    data_dir.with_extension("restoring")
}

/// The journal lives BESIDE `<data>.restoring`, in the parent of the data
/// directory — never inside it. Everything inside the data directory is
/// renamed aside, copied over, or deleted by the very operation the journal
/// exists to describe, so a marker kept there would be destroyed by its own
/// subject.
fn restore_journal_path(data_dir: &Path) -> PathBuf {
    let mut name = restoring_dir_for(data_dir).into_os_string();
    name.push(".journal");
    PathBuf::from(name)
}

/// `fsync` a directory, so that a `rename` or `unlink` performed inside it
/// survives power loss. A file's own `fsync` does not make the directory entry
/// that names it durable.
fn fsync_dir(dir: &Path) -> std::io::Result<()> {
    std::fs::File::open(dir)?.sync_all()
}

/// Publish a restore phase durably.
///
/// A phase marker that is not fsynced does not survive the process death it
/// exists to describe, so this performs the full atomic-durable publish:
/// write a sibling temp file, `fsync` the file, `rename` it over the journal
/// (atomic within one directory), then `fsync` the parent directory so the
/// rename itself is durable. Anything less can leave the journal claiming a
/// phase the filesystem never reached — the same failure this journal exists
/// to remove, pointing the other way.
fn publish_restore_phase(journal: &RestoreJournal) -> anyhow::Result<()> {
    if matches!(
        journal.disposition,
        Some(RestoreDisposition::PreserveLive { .. })
    ) {
        maybe_fail_live_restore_io("journal-publish")?;
    }
    let path = restore_journal_path(&journal.data_dir);
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent)
        .with_context(|| format!("create restore journal directory {}", parent.display()))?;

    let mut tmp = path.clone().into_os_string();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);

    let bytes = serde_json::to_vec_pretty(journal)?;
    {
        use std::io::Write;
        let mut file = std::fs::File::create(&tmp)
            .with_context(|| format!("create restore journal {}", tmp.display()))?;
        file.write_all(&bytes)
            .with_context(|| format!("write restore journal {}", tmp.display()))?;
        file.sync_all()
            .with_context(|| format!("fsync restore journal {}", tmp.display()))?;
    }
    std::fs::rename(&tmp, &path)
        .with_context(|| format!("publish restore journal {}", path.display()))?;
    fsync_dir(parent)
        .with_context(|| format!("fsync restore journal directory {}", parent.display()))?;
    Ok(())
}

/// Test convenience only. Production decoding distinguishes an absent journal
/// from invalid present proof and preserves the explicit legacy phase rules.
#[cfg(test)]
fn read_restore_journal(data_dir: &Path) -> Option<RestoreJournal> {
    read_restore_journal_checked(data_dir).ok().flatten()
}

fn read_restore_journal_checked(data_dir: &Path) -> anyhow::Result<Option<RestoreJournal>> {
    let path = restore_journal_path(data_dir);
    match std::fs::symlink_metadata(&path) {
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    }
    let bytes = read_regular_bounded(&path, 32 * 1024 * 1024)?;
    let raw: serde_json::Value = serde_json::from_slice(&bytes).map_err(restore_format_error)?;
    let journal: RestoreJournal =
        serde_json::from_value(raw.clone()).map_err(restore_format_error)?;
    require_backup!(
        [2, 3, RESTORE_JOURNAL_VERSION].contains(&journal.version),
        "unsupported restore journal; all copies preserved"
    );
    require_backup!(
        absolute_lexically(&journal.data_dir) == absolute_lexically(data_dir)
            && absolute_lexically(&journal.restoring_dir)
                == absolute_lexically(&restoring_dir_for(data_dir)),
        "restore journal namespace mismatch; all copies preserved"
    );
    if journal.version < 4 {
        require_backup!(
            !raw.as_object()
                .is_some_and(|object| object.contains_key("disposition")
                    || object.contains_key("previous_preserved_copies")),
            "legacy restore journal contains a newer disposition; all copies preserved"
        );
    } else {
        let disposition = journal.disposition.as_ref().ok_or_else(|| {
            BackupRefusal(
                "v4 restore journal lacks its mandatory disposition; all copies preserved".into(),
            )
        })?;
        if let RestoreDisposition::PreserveLive { receipt } = disposition {
            require_backup!(
                journal.restoring_identity.is_none()
                    && journal.restoring_paths.is_empty()
                    && journal.restoring_file_witnesses.is_none(),
                "preserve-only proof appeared in deletion domain"
            );
            receipt.validate_paths(data_dir)?;
        }
    }
    Ok(Some(journal))
}

fn retire_live_restore_journal(data_dir: &Path) -> anyhow::Result<()> {
    let path = restore_journal_path(data_dir);
    maybe_fail_live_restore_io("journal-retire")?;
    std::fs::remove_file(&path)?;
    maybe_fail_live_restore_io("journal-retire-fsync")?;
    fsync_dir(path.parent().unwrap_or_else(|| Path::new(".")))?;
    Ok(())
}

/// Durably retire the journal. The unlink is followed by a parent `fsync` for
/// the same reason the publish is: an unlink that is not durable can come back.
fn clear_restore_journal(data_dir: &Path) {
    let path = restore_journal_path(data_dir);
    match std::fs::remove_file(&path) {
        Ok(()) => {
            if let Some(parent) = path.parent() {
                let _ = fsync_dir(parent);
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            tracing::warn!(
                journal = %path.display(),
                "could not remove the restore journal: {error}"
            );
        }
    }
}

/// Make an entire restored dataset durable before anything claims it complete.
///
/// Without this, `CutoverValidated` could be on disk while the bytes it vouches
/// for are still only in the page cache — a marker that survives the crash its
/// data does not is worse than no marker at all.
fn fsync_tree(root: &Path) -> std::io::Result<()> {
    for entry in walkdir::WalkDir::new(root)
        .into_iter()
        .filter_map(Result::ok)
    {
        if entry.file_type().is_file() {
            std::fs::File::open(entry.path())?.sync_all()?;
        } else if entry.file_type().is_dir() {
            fsync_dir(entry.path())?;
        }
    }
    if let Some(parent) = root
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fsync_dir(parent)?;
    }
    Ok(())
}

/// Does `dest` hold a COMPLETE copy of the dataset `manifest` describes?
///
/// Used as a positive proof only, and that asymmetry is deliberate: a dataset
/// that fails here is NOT thereby known to be partial. A restore that succeeded
/// and was then written to by a daemon stops matching its manifest within
/// seconds, which is precisely why a failure here may never authorize deleting
/// a preserved copy.
///
/// `clones/` -> `workspace/`: the restore renames the archive's historical
/// `clones/` directory to the runtime `workspace/` name before cutover, so
/// manifest keys under `clones/` address files that are correctly no longer at
/// that name. Accepting both spellings is not laxity — it is the same file.
fn verify_restored_dataset(dest: &Path, manifest: &BackupManifest) -> anyhow::Result<()> {
    for (filename, expected_hash) in &manifest.checksums {
        let mut path = dest.join(filename);
        if !path.exists()
            && let Some(rest) = filename.strip_prefix("clones/")
        {
            path = dest.join("workspace").join(rest);
        }
        if !path.exists() {
            anyhow::bail!("restored dataset is missing {filename}");
        }
        let actual =
            sha256_stream_path(&path).with_context(|| format!("hashing restored {filename}"))?;
        if actual != *expected_hash {
            anyhow::bail!(
                "restored {filename} does not match the archive: expected {expected_hash}, \
                 got {actual}"
            );
        }
    }
    Ok(())
}

/// Can `dest` prove, from its own embedded `manifest.json`, that it is a
/// complete restored dataset?
///
/// This is the second of the two admissible completion proofs (the journal is
/// the first). It exists so that a `.restoring` left by a build that had no
/// journal, or one whose journal was lost, can still be cleaned up when the
/// replacement really did land — without ever inferring completeness from
/// nonemptiness.
#[cfg(test)]
fn restored_dataset_is_self_proving(dest: &Path) -> bool {
    let Ok(text) = std::fs::read_to_string(dest.join("manifest.json")) else {
        return false;
    };
    let Ok(manifest) = serde_json::from_str::<BackupManifest>(&text) else {
        return false;
    };
    verify_restored_dataset(dest, &manifest).is_ok()
}

/// An interrupted restore copy whose redundancy could not be proven, renamed
/// to a reported sibling rather than deleted.
fn preserve_unproven_copy(restoring_dir: &Path) -> anyhow::Result<PathBuf> {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0);
    let mut candidate = PathBuf::new();
    for attempt in 0..1_000 {
        let mut name = restoring_dir.as_os_str().to_owned();
        if attempt == 0 {
            name.push(format!(".preserved-{stamp}"));
        } else {
            name.push(format!(".preserved-{stamp}-{attempt}"));
        }
        candidate = PathBuf::from(name);
        if !candidate.exists() {
            break;
        }
    }
    std::fs::rename(restoring_dir, &candidate).with_context(|| {
        format!(
            "preserve unverified pre-restore copy {} as {}",
            restoring_dir.display(),
            candidate.display()
        )
    })?;
    if let Some(parent) = candidate.parent() {
        fsync_dir(parent).with_context(|| format!(
            "unproven copy is preserved at {}, but its rename could not be made durable; recovery journal retained",
            candidate.display()
        ))?;
    }
    tracing::warn!(
        preserved = %candidate.display(),
        "an interrupted restore left an unproven copy; it has been preserved rather than deleted"
    );
    Ok(candidate)
}

/// What reconciling a previously interrupted restore did.
#[derive(Debug, Default)]
struct RestoreRecovery {
    /// An unproven copy (incumbent or destination) that was renamed aside
    /// instead of deleted. The caller surfaces this path so the
    /// operator can remove it once satisfied.
    preserved: Option<PathBuf>,
    more_preserved: Vec<PathBuf>,
}

impl RestoreRecovery {
    fn from_paths(paths: Vec<PathBuf>) -> Self {
        let mut unique = Vec::new();
        for path in paths {
            if !unique.contains(&path) {
                unique.push(path);
            }
        }
        Self {
            preserved: unique.first().cloned(),
            more_preserved: unique.into_iter().skip(1).collect(),
        }
    }
    fn paths(&self) -> Vec<PathBuf> {
        self.preserved
            .iter()
            .chain(self.more_preserved.iter())
            .cloned()
            .collect()
    }
}

/// Finish the sole archive graph's physical marker using exact archive proof.
/// Already rebound markers are the only checksum exception: their graph still
/// has to match the archive. No names discovered by a recursive walk confer
/// authority, and a foreign/missing modern marker cannot synthesize ownership.
fn finalize_restored_engine_marker(
    data_dir: &Path,
    journal: &RestoreJournal,
    manifest: &BackupManifest,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        journal_proves_owned_tree(
            data_dir,
            journal.destination_identity,
            &journal.destination_paths
        ),
        "restore marker finalization refused: destination identity or exact inventory changed; both copies preserved"
    );
    let source_checksums = journal.source_checksums.as_ref().context(
        "validated restore journal has no pre-cutover content proof; both copies preserved",
    )?;
    anyhow::ensure!(
        manifest.checksums == *source_checksums,
        "restored manifest differs from pre-cutover content proof; both copies preserved"
    );
    let graph = journal
        .graph_path
        .as_deref()
        .context("validated restore journal has no exact graph role; both copies preserved")?;
    anyhow::ensure!(
        source_checksums.contains_key(graph),
        "graph role lacks a captured checksum"
    );
    anyhow::ensure!(
        Path::new(graph).components().count() == 1
            && normalized_relative_path(data_dir, &data_dir.join(graph))? == graph,
        "restore graph must be a root inventory member"
    );
    let marker = format!(
        "{graph}{}",
        nestweaver_store::engine_format::ENGINE_FORMAT_SIDECAR_SUFFIX
    );
    let db = data_dir.join(graph);
    let already_bound = nestweaver_store::engine_format::has_current_file_marker(&db)?;
    for (filename, expected) in &manifest.checksums {
        let mut path = data_dir.join(filename);
        // Validate components before reading anything, including legacy paths.
        normalized_relative_path(data_dir, &path)?;
        if !path.exists()
            && let Some(rest) = filename.strip_prefix("clones/")
        {
            path = data_dir.join("workspace").join(rest);
        }
        anyhow::ensure!(
            std::fs::symlink_metadata(&path)?.is_file(),
            "restore inventory member is not a regular file: {filename}"
        );
        if filename == &marker && already_bound {
            continue;
        }
        anyhow::ensure!(
            sha256_stream_path(&path)? == *expected,
            "restore marker finalization refused: archive content mismatch for {filename}; both copies preserved"
        );
    }
    if !manifest.checksums.contains_key(&marker) {
        anyhow::ensure!(
            journal.legacy_engine,
            "modern restore is missing its inventoried engine-format marker"
        );
        return Ok(());
    }
    let marker_path = data_dir.join(&marker);
    let marker_text = std::fs::read_to_string(&marker_path)?;
    if marker_text.lines().next() != Some(nestweaver_store::engine_format::ENGINE_FORMAT_VALUE) {
        anyhow::ensure!(
            journal.legacy_engine,
            "modern restore has a foreign engine-format marker"
        );
        return Ok(());
    }
    maybe_crash_restore_step_for_test("before-marker-rebind");
    nestweaver_store::engine_format::restamp_after_copy(&db)
        .with_context(|| format!("durably rebind restored engine marker {}", db.display()))?;
    anyhow::ensure!(
        nestweaver_store::engine_format::has_current_file_marker(&db)?,
        "restored graph marker did not bind the exact copied file"
    );
    maybe_crash_restore_step_for_test("after-marker-rebind");
    Ok(())
}

/// Establish the exact operational lock BEFORE journaling authority over its
/// name. A crash before registration preserves an unproven file; it never
/// grants recovery permission to delete a competing creator's bytes.
fn prepare_restored_write_lock(
    data_dir: &Path,
    journal: &mut RestoreJournal,
) -> anyhow::Result<()> {
    let graph = journal
        .graph_path
        .as_deref()
        .context("restore graph role is absent")?;
    anyhow::ensure!(
        Path::new(graph).components().count() == 1,
        "restore lock requires a root graph inventory member"
    );
    let relative = format!("{graph}.write.lock");
    let path = data_dir.join(&relative);
    normalized_relative_path(data_dir, &path)?;
    let initial = match std::fs::symlink_metadata(&path) {
        Ok(metadata) => {
            if !metadata.is_file() || metadata.len() != 0 {
                refuse_backup!(
                    "backup restore refused: operational lock contains foreign bytes or is not a regular file: {}",
                    path.display()
                );
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;
                if metadata.nlink() != 1 {
                    refuse_backup!(
                        "backup restore refused: operational lock has a foreign hardlink"
                    );
                }
            }
            let witness = restore_file_witness_from_metadata(&metadata)?;
            let source_owned = journal.destination_paths.contains(&relative)
                && journal
                    .source_checksums
                    .as_ref()
                    .and_then(|checksums| checksums.get(&relative))
                    .is_some_and(|hash| {
                        *hash == format!("sha256:{}", hex_encode(&Sha256::digest(b"")))
                    });
            let previously_owned = journal.destination_paths.contains(&relative)
                && journal
                    .destination_file_witnesses
                    .as_ref()
                    .and_then(|files| files.get(&relative))
                    == Some(&witness);
            if !source_owned && !previously_owned {
                refuse_backup!(
                    "backup restore refused: preexisting empty operational lock has no original ownership proof: {}",
                    path.display()
                );
            }
            Some(witness)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error).context("inspect restored operational lock"),
    };
    let file = if let Some(expected) = initial.as_ref() {
        let mut options = std::fs::OpenOptions::new();
        options.read(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.custom_flags(libc::O_NOFOLLOW);
        }
        let file = options.open(&path)?;
        anyhow::ensure!(
            restore_file_witness_from_metadata(&file.metadata()?)? == *expected,
            "operational lock changed during ownership admission"
        );
        file
    } else {
        maybe_inject_operational_lock_race_for_test(&path)?;
        match std::fs::OpenOptions::new()
            .create_new(true)
            .read(true)
            .write(true)
            .open(&path)
        {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                refuse_backup!(
                    "backup restore refused: a foreign operational lock appeared during creation; no ownership authority was recorded"
                );
            }
            Err(error) => return Err(error).context("prepare restored operational lock"),
        }
    };
    let metadata = file.metadata()?;
    anyhow::ensure!(
        metadata.is_file() && metadata.len() == 0,
        "restored operational lock is not an empty regular file"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        anyhow::ensure!(
            metadata.nlink() == 1,
            "restored operational lock has a foreign hardlink"
        );
    }
    file.sync_all()?;
    fsync_dir(data_dir)?;
    anyhow::ensure!(
        restore_file_witness(&path)? == restore_file_witness_from_metadata(&file.metadata()?)?,
        "restored operational lock changed before durable registration"
    );
    if !journal.destination_paths.contains(&relative) {
        journal.destination_paths.push(relative);
        publish_restore_phase(journal)?;
    }
    Ok(())
}

#[cfg(test)]
fn maybe_inject_operational_lock_race_for_test(path: &Path) -> anyhow::Result<()> {
    if std::env::var_os("NW_RESTORE_TEST_OPERATIONAL_LOCK_RACE").is_some() {
        std::fs::write(path, b"foreign creator's irreplaceable bytes")?;
    }
    Ok(())
}

#[cfg(not(test))]
fn maybe_inject_operational_lock_race_for_test(_path: &Path) -> anyhow::Result<()> {
    Ok(())
}

/// Validated recovery uses only the immutable metadata receipt published after
/// archive validation and durable marker finalization. Reading/hashing a leased
/// graph would close a descriptor and release process-wide POSIX writer locks.
/// An older journal cannot prove this boundary: preserve it and both copies.
fn finalize_validated_restore_recovery(
    data_dir: &Path,
    journal: &RestoreJournal,
) -> anyhow::Result<()> {
    let (Some(graph), Some(expected)) = (
        journal.graph_path.as_deref(),
        journal.destination_file_witnesses.as_ref(),
    ) else {
        refuse_backup!(
            "validated restore journal lacks finalized destination proof; data, aside copy and journal preserved for manual recovery"
        );
    };
    if journal.source_checksums.is_none()
        || !journal_proves_complete_owned_tree(
            data_dir,
            journal.destination_identity,
            &journal.destination_paths,
        )
    {
        refuse_backup!(
            "validated restore destination identity or inventory changed; data, aside copy and journal preserved for manual recovery"
        );
    }
    let current = restore_tree_witnesses(data_dir)?;
    if current != *expected || !expected.contains_key(graph) {
        refuse_backup!(
            "validated restore destination file proof changed; data, aside copy and journal preserved for manual recovery"
        );
    }
    if !journal.legacy_engine
        && !nestweaver_store::engine_format::has_current_file_marker(&data_dir.join(graph))?
    {
        refuse_backup!(
            "validated restore destination lacks its finalized physical marker; data, aside copy and journal preserved for manual recovery"
        );
    }
    Ok(())
}

#[cfg(test)]
fn maybe_fail_live_restore_io(step: &str) -> std::io::Result<()> {
    if std::env::var("NW_RESTORE_TEST_LIVE_IO_FAIL").as_deref() == Ok(step) {
        return Err(std::io::Error::other(format!(
            "injected live restore {step} I/O failure"
        )));
    }
    Ok(())
}

#[cfg(not(test))]
fn maybe_fail_live_restore_io(_step: &str) -> std::io::Result<()> {
    Ok(())
}

#[cfg(test)]
fn maybe_crash_restore_step_for_test(label: &str) {
    if std::env::var("NW_RESTORE_TEST_CRASH_AT").as_deref() == Ok(label) {
        // SAFETY: test-only exact process-death seam, no destructors.
        unsafe { libc::_exit(9) };
    }
}

#[cfg(not(test))]
fn maybe_crash_restore_step_for_test(_label: &str) {}

/// Reconcile a leftover `<data>.restoring` from a previously interrupted
/// restore, before starting a new one.
///
/// The restore uses a rename-aside dance: move `data_dir` -> `.restoring`,
/// land the new data at `data_dir`, then delete `.restoring`. What decides
/// whether `.restoring` may be deleted is DURABLE COMPLETION PROOF: the
/// validated journal's original-file witnesses for both copies. A destination's
/// own checksum manifest never authorizes deleting an unproven aside copy.
///
/// Nonemptiness is not proof and never was. A cross-device copy that died
/// after one file leaves `data_dir` nonempty and worthless while `.restoring`
/// holds the user's only complete dataset; the previous version of this
/// function deleted it on exactly that evidence.
fn recover_interrupted_restore(data_dir: &Path) -> anyhow::Result<RestoreRecovery> {
    let journal = read_restore_journal_checked(data_dir)?;
    let mut previous = journal
        .as_ref()
        .map(|journal| journal.previous_preserved_copies.clone())
        .unwrap_or_default();
    let current_context = journal.as_ref().and_then(|journal| match &journal.disposition {
        Some(RestoreDisposition::PreserveLive { receipt }) => Some(format!("known live reservation {}; intended retained dataset {}; original active/aside slots {} and {}", receipt.reservation.display(), receipt.preserved_path.display(), journal.data_dir.display(), journal.restoring_dir.display())),
        _ => None,
    });
    let recovered = recover_interrupted_restore_inner(data_dir, journal).map_err(|error| {
        let error = if let Some(context) = current_context {
            error.context(context)
        } else {
            error
        };
        if previous.is_empty() {
            error
        } else {
            error.context(preserved_paths_message(&previous))
        }
    })?;
    previous.extend(recovered.paths());
    Ok(RestoreRecovery::from_paths(previous))
}

fn retire_unused_live_reservation(
    data_dir: &Path,
    journal: Option<&RestoreJournal>,
) -> anyhow::Result<()> {
    if let Some(RestoreJournal {
        disposition: Some(RestoreDisposition::PreserveLive { receipt }),
        ..
    }) = journal
    {
        receipt.retire_empty_reservation(data_dir)?;
    }
    Ok(())
}

fn preserved_paths_message(paths: &[PathBuf]) -> String {
    paths
        .iter()
        .map(|path| format!("unproven restore copy preserved at {}", path.display()))
        .collect::<Vec<_>>()
        .join("; ")
}

fn recover_interrupted_restore_inner(
    data_dir: &Path,
    journal: Option<RestoreJournal>,
) -> anyhow::Result<RestoreRecovery> {
    let restoring_dir = restoring_dir_for(data_dir);
    let phase = journal.as_ref().map(|journal| journal.phase);
    if let Some(journal) = journal.as_ref()
        && matches!(
            phase,
            Some(RestorePhase::CutoverValidated | RestorePhase::Cleanup)
        )
        && let Some(RestoreDisposition::PreserveLive { receipt }) = &journal.disposition
    {
        finalize_validated_restore_recovery(data_dir, journal)?;
        let preserved = receipt.finish(data_dir)?;
        retire_live_restore_journal(data_dir).with_context(|| {
            format!(
                "retained live dataset at {}; journal retirement failed",
                preserved.display()
            )
        })?;
        return Ok(RestoreRecovery {
            preserved: Some(preserved),
            ..Default::default()
        });
    }

    if !restoring_dir.exists() {
        if matches!(
            phase,
            Some(RestorePhase::CutoverValidated | RestorePhase::Cleanup)
        ) {
            finalize_validated_restore_recovery(data_dir, journal.as_ref().unwrap())?;
        }
        // No incumbent exists. Keep every byte of an unvalidated destination
        // in a reported sibling rather than treating its names as file proof.
        let mut preserved = None;
        if matches!(
            phase,
            Some(
                RestorePhase::CopyInProgress
                    | RestorePhase::OldDataPreserved
                    | RestorePhase::StagingVerified
            )
        ) && data_dir.exists()
        {
            let journal = journal.as_ref().unwrap();
            if journal_proves_owned_tree(
                data_dir,
                journal.destination_identity,
                &journal.destination_paths,
            ) {
                preserved = Some(preserve_unproven_copy(data_dir)?);
            } else if phase != Some(RestorePhase::StagingVerified)
                || !(if matches!(
                    journal.disposition,
                    Some(RestoreDisposition::PreserveLive { .. })
                ) {
                    journal_proves_original_aside(data_dir, journal, true)
                } else {
                    journal_proves_owned_tree(
                        data_dir,
                        journal.restoring_identity,
                        &journal.restoring_paths,
                    )
                })
            {
                refuse_backup!(
                    "backup restore refused: stale or ambiguous recovery journal cannot authorize deleting {}; its contents were preserved",
                    data_dir.display()
                );
            }
            // StagingVerified may precede the rename-aside: the exact recorded
            // incumbent is still at data_dir, so it must remain untouched.
        }
        retire_unused_live_reservation(data_dir, journal.as_ref())?;
        clear_restore_journal(data_dir);
        return Ok(RestoreRecovery {
            preserved,
            ..Default::default()
        });
    }

    match phase {
        // Not authoritative: retain the entire destination, then reinstate
        // the independently proven incumbent. Names/directory identity never
        // authorize deleting potentially replaced destination file contents.
        Some(RestorePhase::CopyInProgress | RestorePhase::OldDataPreserved) => {
            let journal = journal.as_ref().unwrap();
            if !journal_proves_original_aside(&restoring_dir, journal, true) {
                refuse_backup!(
                    "backup restore refused: recovery cannot prove ownership of the preserved aside copy {}; destination, aside copy and journal were preserved",
                    restoring_dir.display()
                );
            }
            let mut preserved = None;
            if data_dir.exists() {
                if !journal_proves_owned_tree(
                    data_dir,
                    journal.destination_identity,
                    &journal.destination_paths,
                ) {
                    refuse_backup!(
                        "backup restore refused: stale or ambiguous recovery journal cannot authorize deleting {}; both copies were preserved",
                        data_dir.display()
                    );
                }
                preserved = Some(preserve_unproven_copy(data_dir)?);
            }
            std::fs::rename(&restoring_dir, data_dir).with_context(|| {
                format!(
                    "recover interrupted restore: rename {} back to {}{}",
                    restoring_dir.display(),
                    data_dir.display(),
                    preserved.as_ref().map_or_else(String::new, |path| format!(
                        "; the unvalidated destination remains preserved at {}",
                        path.display()
                    ))
                )
            })?;
            if let Some(parent) = data_dir.parent().filter(|p| !p.as_os_str().is_empty()) {
                fsync_dir(parent).with_context(|| {
                    format!(
                        "make incumbent promotion durable; journal retained{}",
                        preserved.as_ref().map_or_else(String::new, |path| format!(
                            "; unvalidated destination preserved at {}",
                            path.display()
                        ))
                    )
                })?;
            }
            retire_unused_live_reservation(data_dir, Some(journal))?;
            clear_restore_journal(data_dir);
            Ok(RestoreRecovery {
                preserved,
                ..Default::default()
            })
        }

        // PROVABLY redundant: the journal was published only after the
        // replacement was durable and validated.
        Some(RestorePhase::CutoverValidated | RestorePhase::Cleanup) => {
            let journal = journal.as_ref().unwrap();
            if !journal_proves_original_aside(&restoring_dir, journal, false) {
                refuse_backup!(
                    "backup restore refused: recovery cannot prove original content of the preserved aside copy {}; destination, aside copy and journal were preserved",
                    restoring_dir.display()
                );
            }
            finalize_validated_restore_recovery(data_dir, journal)?;
            std::fs::remove_dir_all(&restoring_dir).with_context(|| {
                format!(
                    "remove the superseded pre-restore copy {}",
                    restoring_dir.display()
                )
            })?;
            clear_restore_journal(data_dir);
            Ok(RestoreRecovery::default())
        }

        // No proof either way — including every `.restoring` written by a
        // build that had no journal at all.
        Some(RestorePhase::StagingVerified) | None => {
            if !dir_is_present(data_dir)
                && journal.as_ref().is_some_and(|journal| {
                    journal_proves_original_aside(&restoring_dir, journal, true)
                })
            {
                // Promotion requires independently captured original-file proof;
                // relocation must never grant a foreign directory force ownership.
                std::fs::rename(&restoring_dir, data_dir).with_context(|| {
                    format!(
                        "recover interrupted restore: rename {} back to {}",
                        restoring_dir.display(),
                        data_dir.display()
                    )
                })?;
                if let Some(parent) = data_dir.parent().filter(|p| !p.as_os_str().is_empty()) {
                    fsync_dir(parent)
                        .context("make incumbent recovery durable before retiring its journal")?;
                }
                retire_unused_live_reservation(data_dir, journal.as_ref())?;
                clear_restore_journal(data_dir);
                Ok(RestoreRecovery::default())
            } else {
                let preserved = preserve_unproven_copy(&restoring_dir)?;
                retire_unused_live_reservation(data_dir, journal.as_ref()).with_context(|| {
                    format!("unproven restore copy preserved at {}", preserved.display())
                })?;
                clear_restore_journal(data_dir);
                Ok(RestoreRecovery {
                    preserved: Some(preserved),
                    ..Default::default()
                })
            }
        }
    }
}

/// True if `dir` exists and contains at least one entry.
///
/// This answers "is there anything here", which is the ONLY question it may be
/// asked. It is not, and must never again be used as, an answer to "did the
/// restore finish" — see [`recover_interrupted_restore`].
fn dir_is_present(dir: &Path) -> bool {
    std::fs::read_dir(dir)
        .map(|mut it| it.next().is_some())
        .unwrap_or(false)
}

/// Force the cross-device copy fallback. Test builds only.
///
/// The atomic rename is taken whenever the kernel allows it, and through the
/// CLI's own path construction the staging directory is almost always a
/// sibling of the target — so the copy fallback, which is the unsafe half of
/// the cutover, is very hard to reach on purpose. This seam makes it
/// reachable in tests without a second filesystem. It is compiled out of every
/// non-test build.
#[cfg(test)]
fn force_cross_device_copy_for_test() -> bool {
    std::env::var_os("NW_RESTORE_TEST_FORCE_COPY").is_some()
}

#[cfg(not(test))]
fn force_cross_device_copy_for_test() -> bool {
    false
}

/// Die at an exact restore phase, with no unwinding, no destructors and no
/// flushing — `_exit`, the closest thing to `SIGKILL` a test can aim
/// precisely. Test builds only; the child process that does the dying is the
/// test binary re-invoked against itself.
#[cfg(test)]
fn maybe_crash_for_test(phase: RestorePhase) {
    let label = match phase {
        RestorePhase::StagingVerified => "staging-verified",
        RestorePhase::OldDataPreserved => "old-data-preserved",
        RestorePhase::CopyInProgress => "copy-in-progress",
        RestorePhase::CutoverValidated => "cutover-validated",
        RestorePhase::Cleanup => "cleanup",
    };
    if std::env::var("NW_RESTORE_TEST_CRASH_AT").as_deref() == Ok(label) {
        // SAFETY: `_exit` is async-signal-safe and terminates immediately.
        unsafe { libc::_exit(9) };
    }
}

#[cfg(not(test))]
fn maybe_crash_for_test(_phase: RestorePhase) {}

/// Die PART WAY THROUGH the cross-device copy, having landed exactly one file,
/// so a test can construct the state the defect was reported against: a
/// destination that is nonempty and worthless beside the only complete copy.
/// Test builds only.
#[cfg(test)]
fn maybe_crash_mid_copy_for_test(source: &Path, dest: &Path) {
    if std::env::var("NW_RESTORE_TEST_CRASH_AT").as_deref() != Ok("copy-midway") {
        return;
    }
    let _ = std::fs::create_dir_all(dest);
    if let Ok(entries) = std::fs::read_dir(source)
        && let Some(first) = entries
            .filter_map(Result::ok)
            .find(|entry| entry.path().is_file())
    {
        let _ = std::fs::copy(first.path(), dest.join(first.file_name()));
    }
    // SAFETY: `_exit` is async-signal-safe and terminates immediately, running
    // no destructor and flushing nothing.
    unsafe { libc::_exit(9) };
}

#[cfg(not(test))]
fn maybe_crash_mid_copy_for_test(_source: &Path, _dest: &Path) {}

/// Corrupt one file of the freshly-copied destination, so a test can drive the
/// post-cutover validation failure and the rollback it triggers. Test builds
/// only.
#[cfg(test)]
fn maybe_corrupt_after_copy_for_test(dest: &Path) {
    if std::env::var_os("NW_RESTORE_TEST_CORRUPT_AFTER_COPY").is_none() {
        return;
    }
    if let Ok(entries) = std::fs::read_dir(dest)
        && let Some(victim) = entries
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| {
                path.is_file()
                    && path
                        .extension()
                        .is_some_and(|extension| extension == "lbug" || extension == "kuzu")
            })
            .min()
    {
        std::fs::write(&victim, b"corrupted by the test").expect("inject copied graph corruption");
        eprintln!("test injected graph corruption into {}", victim.display());
    }
}

#[cfg(not(test))]
fn maybe_corrupt_after_copy_for_test(_dest: &Path) {}

/// Enumerate legacy `.lbug`/`.kuzu` candidates and exact physically marked
/// custom-basename databases directly beneath `data_dir` and
/// its `.restoring` rename-aside sibling — the exact set a restore is about to
/// destroy or has just destroyed, and therefore the exact set that needs a
/// held write lease for the whole operation.
pub fn restore_lease_targets(data_dir: &Path) -> anyhow::Result<Vec<PathBuf>> {
    let mut targets = Vec::new();

    for dir in [data_dir.to_path_buf(), restoring_dir_for(data_dir)] {
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                anyhow::bail!(
                    "cannot enumerate incumbent databases in {} before destructive restore: {error}",
                    dir.display()
                )
            }
        };

        for entry in entries {
            let entry = entry.with_context(|| {
                format!(
                    "cannot enumerate every incumbent database in {} before destructive restore",
                    dir.display()
                )
            })?;
            let path = entry.path();
            let legacy_candidate = matches!(
                path.extension().and_then(|extension| extension.to_str()),
                Some("lbug" | "kuzu")
            );
            if !legacy_candidate
                && !nestweaver_store::engine_format::has_current_file_marker(&path)?
            {
                continue;
            }

            let metadata = std::fs::metadata(&path).with_context(|| {
                format!(
                    "cannot inspect possible incumbent database {} before destructive restore",
                    path.display()
                )
            })?;
            if metadata.is_file() {
                targets.push(nestweaver_store::canonical_db_path(&path));
            }
        }
    }

    // Every restore takes leases in the same canonical order, so two recovery
    // attempts cannot deadlock by discovering directory entries differently.
    targets.sort();
    targets.dedup();
    Ok(targets)
}

/// The exclusive restore authority for one `data_dir`, held for the whole
/// destructive operation.
///
/// `Owned` means this call acquired the namespace and per-database write
/// leases itself and releases them when dropped. `Borrowed` means the
/// calling process already held every namespace AND every per-database write
/// lease this restore needs — the CLI choke point
/// (`with_exclusive_restore_access`) acquires exactly this authority before
/// calling in — so re-acquiring here would self-conflict: the underlying
/// primitives are scoped to the open file description / process-local claim
/// registry, not to "some other call in this process already has it", and
/// neither tolerates a same-process duplicate. Trusting an already-VERIFIED
/// claim is therefore correct, not merely convenient: the authority these
/// leases represent is already in force for the duration of this call either
/// way. A namespace claim ALONE is not enough to license this — see
/// [`acquire_restore_authority`].
#[must_use = "dropping this immediately reopens the window this authority closes"]
enum RestoreAuthority {
    Owned {
        _namespaces: Vec<nestweaver_store::DbNamespaceLease>,
        _leases: Vec<nestweaver_store::DbWriteLease>,
    },
    Borrowed,
}

/// Acquire (or recognize an already-held) exclusive restore authority over
/// every incumbent database beneath `data_dir` and its `.restoring` sibling.
///
/// This is the invariant [`backup_restore`] establishes by construction: a
/// destructive restore never runs without SOME process-held authority
/// excluding every other writer to the exact databases it is about to
/// destroy. Before this, that authority lived only at the CLI choke point —
/// `nestweaver_engine::backup_restore` itself took no lease and was `pub`, so
/// any caller reaching the engine directly (skipping the CLI) bypassed
/// exclusion entirely (nw-375). Acquiring it here closes that gap for every
/// caller, library or CLI, without changing what the CLI already does
/// correctly.
///
/// Recognizing an already-held outer authority (`Borrowed`) must VERIFY that
/// authority, not infer it from the namespace claim alone: a namespace lock
/// proves only that database CREATION is closed for that directory pair, not
/// that any INCUMBENT database's write lease was ever taken. An unrelated
/// in-process caller holding just the namespace lease (via the public
/// `acquire_db_namespace_lease`, for a reason that has nothing to do with
/// restoring anything) would satisfy a namespace-only check while every
/// incumbent `.lbug` underneath remained completely unexcluded — the same
/// "trust ambient state instead of requiring a declaration" shape this sweep
/// keeps finding elsewhere. So every enumerated target must ALSO be a write
/// lease this exact process currently holds before `Borrowed` is trusted;
/// otherwise this falls through to a genuine acquisition attempt, which
/// refuses cleanly (rather than proceeding unprotected) against whichever
/// target turns out not to be this process's own.
fn acquire_restore_authority(data_dir: &Path) -> anyhow::Result<RestoreAuthority> {
    let restoring_dir = restoring_dir_for(data_dir);
    let namespace_dirs = [data_dir.to_path_buf(), restoring_dir];

    if namespace_dirs
        .iter()
        .all(|dir| nestweaver_store::current_process_claims_namespace_lease(dir))
    {
        // The namespace half of the outer authority is in force. Verify the
        // other half before trusting it: every incumbent database this
        // restore is about to destroy must ALSO be a write lease this
        // process already holds. An empty target list (nothing exists to
        // destroy yet) is vacuously satisfied, matching the CLI's own
        // namespace-only protection for a fresh destination.
        let targets = restore_lease_targets(data_dir)?;
        if targets
            .iter()
            .all(|target| nestweaver_store::current_process_claims_write_lease(target))
        {
            return Ok(RestoreAuthority::Borrowed);
        }
        // Verification failed: an outer caller holds the namespace but not
        // every incumbent's write lease. Do NOT proceed unprotected — fall
        // through to a genuine acquisition attempt below, which will refuse
        // cleanly (naming the exact conflict) rather than destroy data no
        // lease of this process's actually covers.
    }

    let targets = restore_lease_targets(data_dir)?;

    let mut namespaces = Vec::with_capacity(namespace_dirs.len());
    for namespace_dir in &namespace_dirs {
        match nestweaver_store::acquire_db_namespace_lease(namespace_dir) {
            Ok(lease) => namespaces.push(lease),
            Err(nestweaver_store::WriteLeaseError::Held) => anyhow::bail!(
                "cannot restore a backup over {}: a writer holds the database namespace write lease for {}; stop every writer and retry",
                data_dir.display(),
                namespace_dir.display()
            ),
            Err(nestweaver_store::WriteLeaseError::Unavailable(error)) => anyhow::bail!(
                "cannot prove exclusive ownership of the database namespace containing {}: {error}; refusing destructive restore",
                namespace_dir.display()
            ),
        }
    }

    // Re-enumerate as an invariant check now that the exclusive namespace
    // authority is held: an upgraded late creator cannot enter after this
    // point, so a changed set here means one raced the first enumeration and
    // this run must not proceed against a stale target list.
    let observed = restore_lease_targets(data_dir)?;
    anyhow::ensure!(
        observed == targets,
        "the incumbent database set changed while restore write leases were being acquired — refusing destructive restore; stop every writer and retry"
    );

    let leases = observed
        .iter()
        .map(|db| {
            let namespace = namespaces
                .iter()
                .find(|namespace| namespace.authorizes(db))
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "no held restore namespace covers incumbent database {}",
                        db.display()
                    )
                })?;
            match nestweaver_store::acquire_db_write_lease_under_namespace(db, namespace) {
                Ok(lease) => Ok(lease),
                Err(nestweaver_store::WriteLeaseError::Held) => anyhow::bail!(
                    "cannot restore a backup over this data directory: another process holds the write lease for {}. Stop the holder first, then retry.",
                    db.display()
                ),
                Err(nestweaver_store::WriteLeaseError::Unavailable(error)) => anyhow::bail!(
                    "cannot take the write lease for {} before destructive restore: {error}",
                    db.display()
                ),
            }
        })
        .collect::<anyhow::Result<Vec<_>>>()?;

    Ok(RestoreAuthority::Owned {
        _namespaces: namespaces,
        _leases: leases,
    })
}

/// Restore a backup archive into a target directory.
///
/// Extracts to a temporary directory first, verifies integrity, then
/// atomically renames to the target. If verification fails, the temp
/// directory is cleaned up and the target is left untouched.
///
/// This holds exclusive restore authority over `config.data_dir` (and its
/// `.restoring` sibling) for the ENTIRE call — acquired first, before
/// [`recover_interrupted_restore`] runs, and released only when this function
/// returns. A caller that already holds that authority (the CLI choke point)
/// is recognized rather than fought; a caller that does not gets it taken out
/// from under it here, by construction, rather than never at all (nw-375).
pub fn backup_restore(config: &RestoreConfig) -> anyhow::Result<RestoreResult> {
    let _authority = acquire_restore_authority(&config.data_dir)?;
    let start = Instant::now();

    let restoring_dir = restoring_dir_for(&config.data_dir);

    // Reconcile a previously interrupted restore FIRST, before this run
    // extracts or touches anything — and do it from the durable journal
    // rather than from "the data directory has something in it". Running it
    // here rather than just before the cutover means a target left broken by
    // an earlier death is repaired even if THIS archive turns out to be
    // invalid.
    // Recovery can rename `<data>.restoring` back into place before the
    // occupancy check. Remember that it had something to reconcile, so a
    // later refusal does not claim the directory was left untouched.
    let interrupted_restore = restoring_dir_for(&config.data_dir).exists();
    let recovery = recover_interrupted_restore(&config.data_dir)?;
    let preserved = recovery.paths();
    let result = restore_archive_under_authority(
        config,
        start,
        restoring_dir,
        recovery,
        interrupted_restore,
        &_authority,
    );
    let result = result.map_err(|error| {
        if let Ok(Some(journal)) = read_restore_journal_checked(&config.data_dir)
            && let Some(RestoreDisposition::PreserveLive { receipt }) = &journal.disposition
        {
            error.context(format!(
                "live retention reservation {}; intended retained dataset {}; journal retained",
                receipt.reservation.display(),
                receipt.preserved_path.display()
            ))
        } else {
            error
        }
    });
    result.map_err(|error| {
        if preserved.is_empty() {
            error
        } else {
            error.context(preserved_paths_message(&preserved))
        }
    })
}

fn restore_archive_under_authority(
    config: &RestoreConfig,
    start: Instant,
    restoring_dir: PathBuf,
    recovery: RestoreRecovery,
    interrupted_restore: bool,
    _authority: &RestoreAuthority,
) -> anyhow::Result<RestoreResult> {
    if !config.replace && restore_target_occupied(&config.data_dir)? {
        if interrupted_restore || recovery.preserved.is_some() {
            let preserved = if recovery.paths().is_empty() {
                String::new()
            } else {
                format!(" {}.", preserved_paths_message(&recovery.paths()))
            };
            refuse_backup!(
                "backup restore refused: {} is not empty. An interrupted restore was reconciled \
                 first, and the archive was not applied.{preserved} Re-run with --force to replace \
                 this directory.",
                config.data_dir.display()
            );
        }
        refuse_backup!(
            "backup restore refused: {} is not empty. Restore renames that directory aside and \
             retains recognized live data outside the active slots. Re-run with --force to replace it. \
             Nothing in {} was changed.",
            config.data_dir.display(),
            config.data_dir.display()
        );
    }
    // A directory an earlier restore was interrupted on was already chosen as
    // a restore target, and recovery has just put its previous contents
    // back; replacing it is what that earlier --force asked for.
    let disposition = if config.replace && restore_target_occupied(&config.data_dir)? {
        replacement_disposition(&config.data_dir)?
    } else {
        ReplacementDisposition::DeleteProvenArchive
    };
    let mut preserved_copies = recovery.paths();

    // Extract to a sibling temp directory so we can atomically rename on success.
    let parent = config
        .data_dir
        .parent()
        .unwrap_or(std::path::Path::new("."));
    std::fs::create_dir_all(parent)?;
    let staging_prefix = restore_staging_prefix(&config.data_dir);
    sweep_dead_pid_leftovers(parent, &staging_prefix, true);
    let temp_dir = tempfile::Builder::new()
        .prefix(&format!("{staging_prefix}{}.", std::process::id()))
        .tempdir_in(parent)?;

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
    let mut manifest: BackupManifest = serde_json::from_str(&manifest_str)?;
    // Set when the archive's graph was built by a storage engine older than
    // LadybugDB 0.21. It is restored byte-for-byte (it is the operator's data
    // and their rollback path), read only through the store's legacy
    // exemption, and the operator is told it must be rebuilt before use.
    let mut pre_cutover_engine = false;
    let mut restored_graph_path = None;

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
        pre_cutover_engine |= publication.format_version < crate::snapshot::SNAPSHOT_FORMAT_VERSION;

        let graph = publication
            .artifacts
            .iter()
            .find(|artifact| artifact.kind == crate::publication::ArtifactKind::Graph)
            .ok_or_else(|| anyhow::anyhow!("backup publication has no graph artifact"))?;
        restored_graph_path = Some(graph.path.clone());
        let graph_store = nestweaver_store::GraphStore::open_read_only_allowing_legacy_engine(
            &temp_dir.path().join(&graph.path),
        )
        .map_err(|error| anyhow::anyhow!("open restored graph identity: {error}"))?;
        pre_cutover_engine |= graph_store.is_legacy_engine();
        validate_restored_logical_identity(&manifest, &graph_store)?;
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

    if manifest.version < 2 {
        // Legacy archives have no typed graph inventory. Accept only an
        // unambiguous database payload and prove its logical ownership too.
        let graphs: Vec<_> = manifest
            .checksums
            .keys()
            .filter(|path| path.ends_with(".lbug") || path.ends_with(".kuzu"))
            .collect();
        let [graph] = graphs.as_slice() else {
            anyhow::bail!(
                "legacy backup has no unambiguous graph payload for logical identity validation"
            );
        };
        restored_graph_path = Some((*graph).clone());
        let store = nestweaver_store::GraphStore::open_read_only_allowing_legacy_engine(
            &temp_dir.path().join(graph),
        )
        .map_err(|error| anyhow::anyhow!("open legacy restored graph identity: {error}"))?;
        validate_restored_logical_identity(&manifest, &store)?;
        pre_cutover_engine |= store.is_legacy_engine();
    }
    if pre_cutover_engine {
        manifest
            .warnings
            .push(pre_cutover_restore_warning(&manifest));
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
    //
    // Every step below is bracketed by a DURABLE phase marker, because the
    // only alternative evidence available to the next run — "the destination
    // has something in it" — is not evidence at all. See [`RestorePhase`].
    let mut journal = RestoreJournal {
        version: RESTORE_JOURNAL_VERSION,
        disposition: Some(RestoreDisposition::DeleteProvenArchive),
        previous_preserved_copies: Vec::new(),
        phase: RestorePhase::StagingVerified,
        data_dir: config.data_dir.clone(),
        restoring_dir: restoring_dir.clone(),
        snapshot_path: config.snapshot_path.clone(),
        destination_identity: restore_directory_identity(temp_dir.path()),
        restoring_identity: restore_directory_identity(&config.data_dir),
        destination_paths: restore_tree_paths(temp_dir.path())?,
        restoring_paths: restore_tree_paths(&config.data_dir)?,
        graph_path: restored_graph_path,
        source_checksums: Some(manifest.checksums.clone()),
        restoring_file_witnesses: Some(restore_tree_witnesses(&config.data_dir)?),
        destination_file_witnesses: None,
        legacy_engine: pre_cutover_engine,
    };
    journal.previous_preserved_copies = recovery.paths();
    if disposition == ReplacementDisposition::PreserveLive {
        require_backup!(
            replacement_disposition(&config.data_dir)? == disposition,
            "live restore layout changed before cutover"
        );
        let receipt = LivePreservation::reserve(&config.data_dir)?;
        journal.restoring_identity = None;
        journal.restoring_paths.clear();
        journal.restoring_file_witnesses = None;
        journal.disposition = Some(RestoreDisposition::PreserveLive {
            receipt: Box::new(receipt),
        });
    }
    publish_restore_phase(&journal).with_context(|| match &journal.disposition {
        Some(RestoreDisposition::PreserveLive { receipt }) => format!(
            "live retention reservation remains at {}; intended retained dataset {}",
            receipt.reservation.display(),
            receipt.preserved_path.display()
        ),
        _ => "publish initial restore journal".into(),
    })?;
    maybe_crash_for_test(RestorePhase::StagingVerified);

    if config.data_dir.exists() {
        // Step 1: Move existing data aside (crash here = old data at .restoring, recoverable).
        std::fs::rename(&config.data_dir, &restoring_dir).with_context(|| {
            format!(
                "rename {} to {}",
                config.data_dir.display(),
                restoring_dir.display()
            )
        })?;
        if let Some(dir) = config
            .data_dir
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
        {
            let _ = fsync_dir(dir);
        }
        maybe_crash_restore_step_for_test("old-data-renamed-before-journal");
        journal.phase = RestorePhase::OldDataPreserved;
        publish_restore_phase(&journal)?;
        maybe_crash_for_test(RestorePhase::OldDataPreserved);
    }

    // Step 2: Move new data into place. The rename is atomic; the copy
    // fallback is not, and everything below exists because of that difference.
    let cross_device_copy = force_cross_device_copy_for_test()
        || std::fs::rename(temp_dir.path(), &config.data_dir).is_err();
    if cross_device_copy {
        std::fs::create_dir_all(&config.data_dir)?;
        journal.destination_identity = restore_directory_identity(&config.data_dir);
        // Published BEFORE the first byte lands. Over-claiming "the
        // destination is not authoritative" costs a retry; under-claiming it
        // costs the user's only complete dataset.
        journal.phase = RestorePhase::CopyInProgress;
        publish_restore_phase(&journal)?;
        maybe_crash_for_test(RestorePhase::CopyInProgress);
        maybe_crash_mid_copy_for_test(temp_dir.path(), &config.data_dir);

        if let Err(e) = nestweaver_storage::copy_dir_all(temp_dir.path(), &config.data_dir) {
            // Partial write may have occurred. The journal already records
            // that, so the next run reinstates the preserved copy rather than
            // deleting it; say so here too, in the terms the operator sees.
            tracing::error!(
                "Cross-device copy failed: {e}. Your previous data is preserved at '{}' and \
                 the next `nestweaver backup restore --data-dir {}` will reinstate it \
                 automatically.",
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
        maybe_corrupt_after_copy_for_test(&config.data_dir);
    }

    // Make the replacement DURABLE before anything claims it is complete: a
    // marker that outlives the data it vouches for is worse than no marker.
    fsync_tree(&config.data_dir).with_context(|| {
        format!(
            "make the restored data at {} durable",
            config.data_dir.display()
        )
    })?;

    // Validate the replacement IN PLACE — but only on the copy path. The
    // rename path moved a directory that was already checksum-verified in
    // staging, atomically; re-hashing a possibly multi-gigabyte dataset there
    // would buy nothing. The copy path has no such guarantee, and it is the
    // one that can land a partial dataset.
    if cross_device_copy && let Err(error) = verify_restored_dataset(&config.data_dir, &manifest) {
        // The destination is not usable. Roll back under the phase that says
        // exactly that, so a death during the rollback still recovers.
        journal.phase = RestorePhase::CopyInProgress;
        publish_restore_phase(&journal)?;
        let recovered = recover_interrupted_restore(&config.data_dir);
        return Err(error.context(format!(
            "restored data at {} failed validation after the cutover{}",
            config.data_dir.display(),
            match recovered {
                Ok(recovery) => format!(
                    "; the pre-restore data has been reinstated{}",
                    if recovery.paths().is_empty() {
                        String::new()
                    } else {
                        format!("; {}", preserved_paths_message(&recovery.paths()))
                    }
                ),
                Err(problem) =>
                    format!("; reinstating the pre-restore data also failed: {problem}"),
            }
        )));
    }

    // Rebinding changes a checksummed marker, so do it only AFTER validating
    // the archive bytes, but BEFORE claiming a complete cutover. A death here
    // remains CopyInProgress (copy) or OldDataPreserved (rename): recovery keeps
    // the incumbent rather than treating a partly finalized copy as complete.
    prepare_restored_write_lock(&config.data_dir, &mut journal)?;
    finalize_restored_engine_marker(&config.data_dir, &journal, &manifest)?;
    journal.destination_file_witnesses = Some(restore_tree_witnesses(&config.data_dir)?);

    journal.phase = RestorePhase::CutoverValidated;
    publish_restore_phase(&journal)?;
    maybe_crash_for_test(RestorePhase::CutoverValidated);

    // Live-layout recognition never grants deletion authority.
    if matches!(
        journal.disposition,
        Some(RestoreDisposition::PreserveLive { .. })
    ) {
        journal.phase = RestorePhase::Cleanup;
        publish_restore_phase(&journal)?;
        maybe_crash_for_test(RestorePhase::Cleanup);
        if let Some(RestoreDisposition::PreserveLive { receipt }) = &journal.disposition {
            preserved_copies.push(receipt.finish(&config.data_dir)?);
        }
    } else if restoring_dir.exists() {
        if !journal_proves_original_aside(&restoring_dir, &journal, false) {
            refuse_backup!(
                "backup restore refused: original aside content changed before cleanup; destination, aside copy and journal were preserved"
            );
        }
        journal.phase = RestorePhase::Cleanup;
        publish_restore_phase(&journal)?;
        maybe_crash_for_test(RestorePhase::Cleanup);
        if let Err(error) = std::fs::remove_dir_all(&restoring_dir) {
            tracing::warn!(
                path = %restoring_dir.display(),
                "the superseded pre-restore copy could not be removed: {error}"
            );
        }
    }
    if matches!(
        journal.disposition,
        Some(RestoreDisposition::PreserveLive { .. })
    ) {
        retire_live_restore_journal(&config.data_dir).with_context(|| {
            format!(
                "retained datasets: {}; journal retirement failed",
                preserved_copies
                    .iter()
                    .map(|path| path.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        })?;
    } else {
        clear_restore_journal(&config.data_dir);
    }

    Ok(RestoreResult {
        manifest,
        data_dir: config.data_dir.clone(),
        duration: start.elapsed(),
        preserved_copy: preserved_copies.first().cloned(),
        preserved_copies,
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
        let present = if *suffix == ".gitactivity.json" {
            // A broken symlink or inaccessible payload is not an absent cache.
            // Preserve the error instead of silently dropping activity scores.
            match std::fs::symlink_metadata(&sidecar) {
                Ok(_) => true,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
                Err(error) => return Err(error).context("inspect git-activity sidecar for backup"),
            }
        } else {
            sidecar.exists()
        };
        if !present {
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
) -> anyhow::Result<(crate::publication::PublicationBundleV3, Vec<String>)> {
    build_backup_publication_bundle_inner(config, staging, identity, source_graph_generation, false)
}

fn build_backup_publication_bundle_inner(
    config: &BackupConfig,
    staging: &Path,
    identity: &nestweaver_store::PublicationIdentity,
    source_graph_generation: u64,
    live_writer: bool,
) -> anyhow::Result<(crate::publication::PublicationBundleV3, Vec<String>)> {
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
        if live_writer && path == format!("{db_filename}.write.lock") {
            // Only the live-slot caller with exact authority enables this.
            // Archive staging still classifies this name as an error.
            continue;
        }
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
                if nestweaver_store::artifact_envelope::is_rebuildable_artifact(&message) {
                    // Drop it from STAGING too, not just from the bundle: the
                    // archive inventory is exact, so a file present on disk
                    // and absent from the manifest fails verification.
                    // A producer-only mismatch is the same class as a stale
                    // generation: the payload is rebuildable derived data, and
                    // readers already refuse it. Foreign identity, a corrupt
                    // checksum, and an incompatible algorithm stay fatal.
                    std::fs::remove_file(entry.path()).with_context(|| {
                        format!("exclude rebuildable artifact {}", entry.path().display())
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
            "backup excluded {} rebuildable derived artifact(s); the graph \
             itself is backed up in full and these are regenerated by the next \
             index:\n  {}",
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
    Ok((bundle, excluded))
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

/// Flat v1 scores cannot be assigned to repositories without guessing.
/// Corrupt and unknown-version data remain hard errors, not legacy exclusions.
fn legacy_git_activity_payload(payload: &[u8]) -> anyhow::Result<bool> {
    let shape: serde_json::Value =
        serde_json::from_slice(payload).context("invalid git-activity JSON")?;
    if shape.get("version").is_some() {
        let current: crate::git_activity::GitActivitySidecar =
            serde_json::from_value(shape).context("invalid versioned git-activity payload")?;
        anyhow::ensure!(
            current.version == crate::git_activity::GITACTIVITY_VERSION,
            "unsupported git-activity sidecar version {}; expected {}",
            current.version,
            crate::git_activity::GITACTIVITY_VERSION
        );
        return Ok(false);
    }
    if serde_json::from_slice::<HashMap<String, f64>>(payload).is_ok() {
        return Ok(true);
    }
    anyhow::bail!(
        "invalid git-activity payload; expected a versioned repository map or legacy flat scores"
    )
}

fn backup_artifact_contract_for_path_inner(
    path: &str,
    db_filename: &str,
    absolute: &Path,
    identity: &nestweaver_store::PublicationIdentity,
    source_graph_generation: u64,
) -> anyhow::Result<(crate::publication::ArtifactKind, u32, String)> {
    match path.strip_prefix(db_filename) {
        Some(".gitactivity.json") => {
            anyhow::ensure!(
                !legacy_git_activity_payload(&std::fs::read(absolute)?)?,
                "legacy git-activity payload requires explicit exclusion during backup packaging"
            );
            return Ok((
                crate::publication::ArtifactKind::GitActivity,
                crate::git_activity::GITACTIVITY_VERSION,
                "nestweaver-git-activity-v2".to_string(),
            ));
        }

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
            Some(".parsed_cache.log") => (
                ArtifactKind::ParsedCache,
                1,
                "nestweaver-parsed-cache-log-v1",
            ),
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
            // nw-670 review L7: the link-rules version the stored links were
            // built with, plus any owed-link debt. A compatibility stamp like
            // the resolver generation: it describes how the graph's edges
            // were built, so it travels with them.
            Some(crate::code_links::CODE_LINKS_SIDECAR) => (
                ArtifactKind::CompatibilityStamp,
                1,
                "nestweaver-code-links-v1",
            ),
            // Whether the name-inferred cross-repo links are owed a
            // whole-graph pass: it describes the graph's edges, so it
            // travels with them, like the code-links stamp.
            Some(crate::cross_repo_links::CROSS_REPO_LINKS_SIDECAR) => (
                ArtifactKind::CompatibilityStamp,
                1,
                "nestweaver-cross-repo-links-v1",
            ),
            // Every vault index writes these two beside the graph, so a
            // rebuilt slot that holds a vault carries both. Neither is on the
            // backup list (a restored registration record would claim vaults
            // the restored file never published), but a publication slot IS
            // the graph that indexed its vaults: they describe it exactly and
            // are sealed with it. Unclassified, they refused every rebuild of
            // a brain with a vault.
            Some(crate::vault_registration::VAULT_REGISTRATIONS_SUFFIX) => (
                ArtifactKind::CompatibilityStamp,
                1,
                "nestweaver-vault-registrations-v1",
            ),
            // A rebuild stamps each vault it indexed in full as derived by
            // the current Markdown link rules, bound to the slot's identity.
            Some(crate::markdown_derivation::RECORD_SUFFIX) => (
                ArtifactKind::CompatibilityStamp,
                1,
                "nestweaver-markdown-derivation-v1",
            ),
            Some(crate::index_md::SKIPPED_NOTES_SIDECAR_SUFFIX) => {
                (ArtifactKind::FileMetadata, 1, "nestweaver-skipped-notes-v1")
            }
            Some(".filemeta.json") => {
                (ArtifactKind::FileMetadata, 1, "nestweaver-file-metadata-v1")
            }
            // Which storage-engine string hash built the graph file beside it.
            // A compatibility stamp, like the resolver generation: it describes
            // the graph, so it travels with it.
            Some(nestweaver_store::engine_format::ENGINE_FORMAT_SIDECAR_SUFFIX) => (
                ArtifactKind::CompatibilityStamp,
                1,
                "nestweaver-engine-format-v1",
            ),
            Some(".manifests.json") => anyhow::bail!(
                "repository manifest contract requires payload inspection; use backup_artifact_contract_for_payload"
            ),
            // v2: repo-keyed. Bumped with the format, or a restore would
            // reintroduce a v1 flat payload under a v2 contract claim — the
            // artifact vouching for a shape it does not have.
            Some(".gitactivity.json") => anyhow::bail!(
                "git-activity contract requires payload inspection; use backup_artifact_contract_for_path"
            ),
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
        warnings: Vec::new(),
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

fn validate_restored_logical_identity(
    manifest: &BackupManifest,
    store: &nestweaver_store::GraphStore,
) -> anyhow::Result<()> {
    let logical = backup_logical_instance_id(store)?;
    if manifest.instance_id != logical {
        anyhow::bail!(
            "backup logical instance identity mismatch: manifest '{}', graph '{}'; restore refused before cutover",
            manifest.instance_id,
            logical
        );
    }
    Ok(())
}

/// Whether `path` already holds something restore would rename aside.
/// `--force` replaces a data directory, which ends in deleting it. Only do
/// that to a directory whose database has an exact physical format marker
/// and whose complete contents belong to known database artifacts. Any other
/// non-empty directory (a home directory, a
/// source tree, a typo) is refused even with `--force`, because the cost of
/// guessing wrong is the user's files.
#[cfg(test)]
fn refuse_replacing_foreign_directory(data_dir: &Path) -> anyhow::Result<()> {
    replacement_disposition(data_dir).map(|_| ())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReplacementDisposition {
    DeleteProvenArchive,
    PreserveLive,
}

fn replacement_disposition(data_dir: &Path) -> anyhow::Result<ReplacementDisposition> {
    if is_recognisable_data_dir(data_dir)? {
        return Ok(ReplacementDisposition::DeleteProvenArchive);
    }
    if recognisable_data_dir(data_dir, true)? {
        return Ok(ReplacementDisposition::PreserveLive);
    }
    refuse_backup!(
        "backup restore refused: {} has unknown or unverified contents; --force does not authorize replacing them. Restore into an empty directory. Nothing was changed.",
        data_dir.display()
    )
}

fn recognised_live_sidecar(data_dir: &Path, graph: &str, name: &str) -> anyhow::Result<bool> {
    let path = data_dir.join(name);
    if name == format!("{graph}.publications") {
        let mut entries = std::fs::read_dir(&path)?;
        let Some(entry) = entries.next().transpose()? else {
            return Ok(false);
        };
        let metadata = std::fs::symlink_metadata(entry.path())?;
        let no_more_entries = entries.next().transpose()?.is_none();
        return Ok(no_more_entries
            && entry.file_name() == "LOCK"
            && regular_unique_file(&metadata)
            && metadata.len() == 0);
    }
    if name == format!("{graph}.tantivy") {
        return nestweaver_store::tantivy_index::is_preservable_live_layout(&path);
    }
    if name == format!("{graph}.markdown-derivation.json") {
        let bundle: crate::publication::PublicationBundleV3 = serde_json::from_slice(
            &bounded_live_read(&data_dir.join(crate::publication::PUBLICATION_MANIFEST_FILE))?,
        )?;
        bundle.validate_metadata(bundle.format_version)?;
        let manifest: BackupManifest =
            serde_json::from_slice(&bounded_live_read(&data_dir.join("manifest.json"))?)?;
        let identity = nestweaver_store::PublicationIdentity {
            brain_uuid: bundle.brain_uuid,
            publication_uuid: bundle.publication_uuid,
        };
        let records = crate::markdown_derivation::decode_records(
            &bounded_live_read(&path)?,
            &crate::markdown_derivation::RecordExpectation {
                identity: &identity,
                data_instance_id: &manifest.instance_id,
            },
        )?;
        return Ok(records.vaults.values().all(|record| {
            record.derivation_version <= crate::markdown_derivation::DERIVATION_VERSION
        }));
    }
    if name == format!("{graph}.skipped_notes.json") {
        let bytes = bounded_live_read(&path)?;
        let value: serde_json::Value = serde_json::from_slice(&bytes)?;
        let allowed = [
            "version",
            "skipped",
            "notes_near_size_limit",
            "skipped_truncated",
            "near_limit_truncated",
            "unindexable_mtimes",
            "reconciliation_pending",
            "frontmatter_unparsed",
        ];
        if !value
            .as_object()
            .is_some_and(|object| object.keys().all(|key| allowed.contains(&key.as_str())))
        {
            return Ok(false);
        }
        let record: crate::index_md::SkippedNotesSidecar = serde_json::from_slice(&bytes)?;
        return Ok(record.version == 1);
    }
    Ok(false)
}

fn regular_unique_file(metadata: &std::fs::Metadata) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        metadata.is_file() && metadata.nlink() == 1
    }
    #[cfg(not(unix))]
    {
        let _ = metadata;
        false
    }
}

fn bounded_live_read(path: &Path) -> anyhow::Result<Vec<u8>> {
    read_regular_bounded(path, 8 * 1024 * 1024)
}

fn read_regular_bounded(path: &Path, limit: u64) -> anyhow::Result<Vec<u8>> {
    let metadata = std::fs::symlink_metadata(path)?;
    require_backup!(
        regular_unique_file(&metadata) && metadata.len() <= limit,
        "invalid or oversized descriptor: {}",
        path.display()
    );
    use std::io::Read;
    let mut bytes = Vec::new();
    std::fs::File::open(path)?
        .take(limit + 1)
        .read_to_end(&mut bytes)?;
    require_backup!(bytes.len() as u64 <= limit, "descriptor exceeds its bound");
    Ok(bytes)
}

fn is_recognisable_data_dir(data_dir: &Path) -> anyhow::Result<bool> {
    recognisable_data_dir(data_dir, false)
}

fn recognisable_data_dir(data_dir: &Path, live: bool) -> anyhow::Result<bool> {
    if !std::fs::symlink_metadata(data_dir)?.is_dir() {
        return Ok(false);
    }
    let entries = std::fs::read_dir(data_dir)?
        .take(100_001)
        .collect::<Result<Vec<_>, _>>()?;
    if entries.len() > 100_000 {
        return Ok(false);
    }
    // A filename or copied manifest is not evidence that arbitrary adjacent
    // files belong to NestWeaver. Require an inode-bound engine marker and an
    // exhaustive set of owned top-level artifacts before recursive deletion.
    let databases: Vec<String> = entries
        .iter()
        .filter_map(|entry| {
            let name = entry.file_name().into_string().ok()?;
            if entry.file_type().ok()?.is_file()
                && nestweaver_store::engine_format::has_current_file_marker(&entry.path()).ok()?
            {
                Some(name)
            } else {
                None
            }
        })
        .collect();
    if databases.is_empty() {
        return Ok(false);
    }
    if live && databases.len() != 1 {
        return Ok(false);
    }
    let manifest_path = data_dir.join(crate::publication::PUBLICATION_MANIFEST_FILE);
    let inventory = if manifest_path.exists() {
        if std::fs::symlink_metadata(&manifest_path)?.len() > MAX_PUBLICATION_MANIFEST_BYTES {
            return Ok(false);
        }
        let Ok(bundle) = serde_json::from_slice::<crate::publication::PublicationBundleV3>(
            &std::fs::read(&manifest_path)?,
        ) else {
            return Ok(false);
        };
        if ![
            crate::snapshot::SNAPSHOT_FORMAT_VERSION,
            crate::snapshot::LEGACY_ENGINE_SNAPSHOT_FORMAT_VERSION,
        ]
        .contains(&bundle.format_version)
            || bundle.validate_metadata(bundle.format_version).is_err()
        {
            return Ok(false);
        }
        bundle
            .artifacts
            .into_iter()
            .map(|artifact| artifact.path)
            .collect::<std::collections::HashSet<_>>()
    } else {
        std::collections::HashSet::new()
    };
    for entry in entries {
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            return Ok(false);
        };
        if entry.file_type()?.is_symlink() {
            return Ok(false);
        }
        if live {
            let metadata = std::fs::symlink_metadata(entry.path())?;
            if metadata.is_file() && !regular_unique_file(&metadata) {
                return Ok(false);
            }
            if [
                ".publications",
                ".tantivy",
                ".markdown-derivation.json",
                ".skipped_notes.json",
            ]
            .iter()
            .any(|suffix| name == format!("{}{suffix}", databases[0]))
            {
                match recognised_live_sidecar(data_dir, &databases[0], &name) {
                    Ok(true) => continue,
                    Ok(false) => return Ok(false),
                    Err(error) if error.downcast_ref::<std::io::Error>().is_some() => {
                        return Err(error);
                    }
                    Err(error) => {
                        return Err(error.context(BackupRefusal(format!(
                            "unsupported or invalid live descriptor {name}; all files preserved"
                        ))));
                    }
                }
            }
            if name == format!("{}.write.lock", databases[0]) && metadata.len() != 0 {
                return Ok(false);
            }
        }
        if databases.contains(&name) {
            continue;
        }
        let owned_sidecar = databases.iter().any(|db| {
            name.strip_prefix(db.as_str()).is_some_and(|suffix| {
                SIDECAR_SUFFIXES.contains(&suffix)
                    || [
                        ".wal",
                        ".write.lock",
                        ".publications",
                        ".vault-registrations.json",
                    ]
                    .contains(&suffix)
            })
        });
        if owned_sidecar && entry.file_type()?.is_file() {
            continue;
        }
        if name == "manifest.json" {
            if serde_json::from_slice::<BackupManifest>(&std::fs::read(entry.path())?).is_err() {
                return Ok(false);
            }
            continue;
        }
        if name == crate::publication::PUBLICATION_MANIFEST_FILE {
            continue;
        }
        // Full archives include instance.toml and clone workspaces. Their
        // names alone confer no ownership: every payload must be inventoried,
        // including files nested below a recognized directory.
        if inventory.contains(&name) && entry.file_type()?.is_file() {
            continue;
        }
        if entry.file_type()?.is_dir() {
            let prefix = format!("{name}/");
            if inventory.iter().any(|path| path.starts_with(&prefix)) {
                for child in walkdir::WalkDir::new(entry.path()).min_depth(1) {
                    let child = child?;
                    if child.file_type().is_symlink() {
                        return Ok(false);
                    }
                    if child.file_type().is_file()
                        && !inventory.contains(&normalized_relative_path(data_dir, child.path())?)
                    {
                        return Ok(false);
                    }
                    if child.file_type().is_dir() {
                        let prefix =
                            format!("{}/", normalized_relative_path(data_dir, child.path())?);
                        if !inventory.iter().any(|path| path.starts_with(&prefix)) {
                            return Ok(false);
                        }
                    }
                }
                continue;
            }
        }
        return Ok(false);
    }
    Ok(true)
}

fn has_database_header(path: &Path) -> std::io::Result<bool> {
    use std::io::Read;
    let mut header = [0; 4];
    let mut file = std::fs::File::open(path)?;
    match file.read_exact(&mut header) {
        Ok(()) => Ok(&header == b"LBUG" || &header == b"KUZU"),
        Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => Ok(false),
        Err(error) => Err(error),
    }
}

/// Refuse a backup destination that IS the database or one of its files.
///
/// `--force` publishes by renaming over the destination, so `backup save
/// <db> --force` replaced the live graph with an archive. The database's
/// footprint is the database path and every `<db>.<suffix>` / `<db>-<suffix>`
/// sibling (WAL, lock, tantivy, generation, publications, ...), including
/// anything beneath one of those. The check is made on canonical paths, so a
/// symlink to the database is caught, and on (device, inode), so a hard link
/// to any existing footprint file is caught too.
fn refuse_output_in_database_footprint(db_path: &Path, output: &Path) -> anyhow::Result<()> {
    let db = std::fs::canonicalize(db_path).unwrap_or_else(|_| absolute_lexically(db_path));
    let (Some(db_parent), Some(db_name)) = (db.parent(), db.file_name()) else {
        return Ok(());
    };
    let db_name = db_name.to_string_lossy().into_owned();
    let refuse = || -> anyhow::Result<()> {
        refuse_backup!(
            "backup refused: the destination {} is the database {} or one of its files. Choose \
             a path outside the database's files; nothing was written.",
            output.display(),
            db.display()
        )
    };
    let in_footprint = |name: &str| {
        name == db_name
            || name
                .strip_prefix(db_name.as_str())
                .is_some_and(|rest| rest.starts_with('.') || rest.starts_with('-'))
    };

    // A sibling named like a sidecar is refused even when it does not exist
    // yet (an archive saved as `<db>.wal` would later be read as the WAL),
    // except for an archive name: no sidecar ends `.nwsnap.zst`, and a backup
    // called `<db>.2026-10-21.nwsnap.zst` beside the database is legitimate.
    let resolved = resolve_output_path(output);
    // A different database's sidecars are destructive destinations too.
    for ancestor in resolved.ancestors() {
        let Some(parent) = ancestor.parent() else {
            continue;
        };
        let Some(name) = ancestor.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if let Ok(entries) = std::fs::read_dir(parent) {
            for entry in entries.flatten() {
                let candidate = entry.file_name();
                let Some(db_name) = candidate.to_str() else {
                    continue;
                };
                if (db_name.ends_with(".lbug") || db_name.ends_with(".kuzu"))
                    && name
                        .strip_prefix(db_name)
                        .is_some_and(|suffix| suffix.starts_with('.') || suffix.starts_with('-'))
                    && !name.ends_with(".nwsnap.zst")
                {
                    return refuse();
                }
            }
        }
    }

    if let Ok(rest) = resolved.strip_prefix(db_parent) {
        let mut components = rest.components();
        if let Some(std::path::Component::Normal(first)) = components.next() {
            let first = first.to_string_lossy();
            let archive_name = components.next().is_none() && first.ends_with(".nwsnap.zst");
            if in_footprint(&first) && (first == db_name || !archive_name) {
                return refuse();
            }
        }
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if let Ok(target) = std::fs::metadata(output) {
            let same = |path: &Path| {
                std::fs::metadata(path)
                    .is_ok_and(|meta| meta.dev() == target.dev() && meta.ino() == target.ino())
            };
            if same(&db) {
                return refuse();
            }
            if let Ok(entries) = std::fs::read_dir(db_parent) {
                for entry in entries.flatten() {
                    if in_footprint(&entry.file_name().to_string_lossy()) && same(&entry.path()) {
                        return refuse();
                    }
                }
            }
        }
    }
    // Protect every database, not only the source. Magic catches a database
    // hidden behind a differently named hard link; canonical paths catch links.
    if [output, resolved.as_path()].iter().any(|path| {
        path.extension()
            .is_some_and(|ext| ext == "lbug" || ext == "kuzu")
    }) || (resolved.is_file() && has_database_header(&resolved)?)
    {
        return refuse();
    }

    Ok(())
}

/// The path `output` names once symlinks are followed: canonical when it
/// exists, else its canonical parent joined with its file name.
fn resolve_output_path(output: &Path) -> PathBuf {
    if let Ok(canonical) = std::fs::canonicalize(output) {
        return canonical;
    }
    let absolute = absolute_lexically(output);
    match (absolute.parent(), absolute.file_name()) {
        (Some(parent), Some(name)) => std::fs::canonicalize(parent)
            .map(|parent| parent.join(name))
            .unwrap_or(absolute),
        _ => absolute,
    }
}

fn absolute_lexically(path: &Path) -> PathBuf {
    std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf())
}

fn restore_target_occupied(path: &Path) -> anyhow::Result<bool> {
    match std::fs::read_dir(path) {
        Ok(mut entries) => Ok(entries.next().is_some()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) if error.kind() == std::io::ErrorKind::NotADirectory => Ok(true),
        Err(error) => {
            Err(error).with_context(|| format!("inspect restore target {}", path.display()))
        }
    }
}

/// Write the archive beside `output`, then publish it.
///
/// The destination is never truncated in place. Without `overwrite`, an
/// existing destination is left byte-for-byte alone, including a file that
/// appears after the first existence check. `rename` would replace that file.
fn publish_backup_archive(staging: &Path, output: &Path, overwrite: bool) -> anyhow::Result<()> {
    if !overwrite && output.exists() {
        archive_exists(output)?;
    }
    let temp = archive_partial_path(output);
    if let Some(parent) = temp
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent)?;
    }
    if let Some(name) = output.file_name() {
        let parent = output
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        let prefix = format!("{}.partial.", name.to_string_lossy());
        sweep_dead_pid_leftovers(parent, &prefix, false);
    }
    let packaged = package_tar_zstd(staging, &temp);
    if packaged.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    packaged?;
    let published = if overwrite {
        publish_replacing(&temp, output)
    } else if output.exists() {
        archive_exists(output)
    } else {
        publish_exclusive(&temp, output)
    };
    if published.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    published?;
    let _ = std::fs::remove_file(&temp);
    if let Some(parent) = output
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        let _ = fsync_dir(parent);
    }
    Ok(())
}

fn archive_exists(output: &Path) -> anyhow::Result<()> {
    refuse_backup!(
        "backup refused: {} already exists; re-run with --force to overwrite it. \
         The existing archive was not modified.",
        output.display()
    );
}

/// Replace `output` with `temp`. A failed rename must not fall back to a copy
/// that truncates an archive already at `output`.
fn publish_replacing(temp: &Path, output: &Path) -> anyhow::Result<()> {
    match std::fs::rename(temp, output) {
        Ok(()) => Ok(()),
        Err(error) if output.exists() => Err(error).with_context(|| {
            format!(
                "publish backup archive to {}; the existing archive was not modified",
                output.display()
            )
        }),
        Err(error) => copy_new_file(temp, output).with_context(|| {
            format!(
                "publish backup archive to {} after rename failed: {error}",
                output.display()
            )
        }),
    }
}

/// Publish without replacing. `hard_link` fails if `output` appeared; `rename`
/// would have overwritten it.
fn publish_exclusive(temp: &Path, output: &Path) -> anyhow::Result<()> {
    match std::fs::hard_link(temp, output) {
        Ok(()) => Ok(()),
        Err(error) if is_already_exists(&error) => archive_exists(output),
        Err(error) => match copy_new_file(temp, output) {
            Ok(()) => Ok(()),
            Err(copy_error) if is_already_exists(&copy_error) => archive_exists(output),
            Err(copy_error) => Err(copy_error).with_context(|| {
                format!(
                    "publish backup archive to {} after link failed: {error}",
                    output.display()
                )
            }),
        },
    }
}

fn copy_new_file(from: &Path, to: &Path) -> std::io::Result<()> {
    let mut source = std::fs::File::open(from)?;
    let mut dest = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(to)?;
    std::io::copy(&mut source, &mut dest)?;
    dest.sync_all()?;
    Ok(())
}

fn is_already_exists(error: &std::io::Error) -> bool {
    error.kind() == std::io::ErrorKind::AlreadyExists
}

/// Prefix of a restore's staging directory beside `data_dir`; the pid that
/// made it follows, so a later restore can tell a dead run's leftover from a
/// live one's.
fn restore_staging_prefix(data_dir: &Path) -> String {
    let name = data_dir.file_name().map_or_else(
        || "data".to_string(),
        |name| name.to_string_lossy().into_owned(),
    );
    format!(".{name}.restore-staging.")
}

/// Remove `<prefix><pid>.<rest>` entries in `dir` whose pid is not alive.
///
/// A killed `backup save` left `<archive>.partial.<pid>.<n>` files and a
/// killed restore left its staging directory; nothing ever removed them, so
/// each kill leaked a full-size copy. Only an entry whose pid parses and is
/// provably gone is removed (liveness fails closed), and never this process's
/// own. Best effort: a failure here must not fail the save or restore.
fn sweep_dead_pid_leftovers(dir: &Path, prefix: &str, directories: bool) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(rest) = name.to_str().and_then(|name| name.strip_prefix(prefix)) else {
            continue;
        };
        let Some(pid) = rest
            .split('.')
            .next()
            .and_then(|pid| pid.parse::<i32>().ok())
        else {
            continue;
        };
        if pid <= 0
            || pid as u32 == std::process::id()
            || crate::index_publication::process_is_alive(pid)
        {
            continue;
        }
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        let path = entry.path();
        let removed = if directories && kind.is_dir() {
            std::fs::remove_dir_all(&path)
        } else if !directories && kind.is_file() {
            std::fs::remove_file(&path)
        } else {
            continue;
        };
        match removed {
            Ok(()) => {
                tracing::info!(path = %path.display(), pid, "removed leftover of a dead backup/restore")
            }
            Err(error) => {
                tracing::warn!(path = %path.display(), %error, "could not remove leftover of a dead backup/restore")
            }
        }
    }
}

fn archive_partial_path(output: &Path) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static PARTIAL_SEQ: AtomicU64 = AtomicU64::new(0);
    let mut name = output
        .file_name()
        .unwrap_or(std::ffi::OsStr::new("backup.nwsnap.zst"))
        .to_os_string();
    // A unique name, not a stable `*.partial`. Exclusive publish links this
    // file onto `output`; a crash before the link is dropped must not leave a
    // name the next save will truncate, because that truncation would hit the
    // published archive too.
    name.push(format!(
        ".partial.{}.{}",
        std::process::id(),
        PARTIAL_SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    match output.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.join(name),
        _ => PathBuf::from(name),
    }
}

#[cfg(test)]
fn leftover_partials(output: &Path) -> bool {
    let Some(parent) = output
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    else {
        return false;
    };
    let Some(prefix) = output.file_name() else {
        return false;
    };
    let prefix = prefix.to_string_lossy();
    std::fs::read_dir(parent)
        .ok()
        .into_iter()
        .flatten()
        .flatten()
        .any(|entry| {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            name.starts_with(prefix.as_ref()) && name.contains(".partial.")
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
    let file = encoder.finish()?;
    file.sync_all()?;
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

/// The warning a restore of a pre-cutover archive carries.
pub fn pre_cutover_restore_warning(manifest: &BackupManifest) -> String {
    format!(
        "this archive (written by NestWeaver {}) holds a database built by a storage \
         engine older than LadybugDB 0.21. It was restored unchanged, but this version refuses to open the \
         restored database (nestweaver::db_rebuild_required) until it is rebuilt: stop the \
         daemon and run `nestweaver publication rebuild --config <instance.toml>`, or use \
         the NestWeaver version that wrote the archive",
        manifest.nestweaver_version
    )
}

fn validate_backup_publication_inventory(
    manifest: &BackupManifest,
    publication: &crate::publication::PublicationBundleV3,
    file_sizes: &HashMap<String, u64>,
    file_hashes: &HashMap<String, String>,
) -> anyhow::Result<()> {
    // A pre-cutover archive (the last format before the storage-engine
    // change) is still restorable; the caller warns that it must be rebuilt.
    publication.validate_metadata(
        if publication.format_version == crate::snapshot::LEGACY_ENGINE_SNAPSHOT_FORMAT_VERSION {
            crate::snapshot::LEGACY_ENGINE_SNAPSHOT_FORMAT_VERSION
        } else {
            crate::snapshot::SNAPSHOT_FORMAT_VERSION
        },
    )?;
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
    fn git_activity_backup_round_trip_preserves_v2_and_excludes_legacy_honestly() {
        for (payload, legacy) in [
            (r#"{"src/main.rs":0.9}"#, true),
            (
                r#"{"version":2,"repos":{"repo:test":{"src/main.rs":0.9}}}"#,
                false,
            ),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let db = dir.path().join("brain.lbug");
            drop(nestweaver_store::GraphStore::open(&db).unwrap());
            let activity = crate::sidecar_path(&db, ".gitactivity.json");
            std::fs::write(&activity, payload).unwrap();
            let output = dir.path().join("backup.nwsnap.zst");
            let saved = backup_save(&BackupConfig {
                db_path: db,
                output_path: output.clone(),
                include_clones: false,
                instance_id: "test".into(),
                workspace_path: None,
                overwrite: false,
            })
            .unwrap();
            assert_eq!(
                std::fs::read_to_string(&activity).unwrap(),
                payload,
                "live source is untouched"
            );
            assert_eq!(saved.manifest.warnings.len(), usize::from(legacy));
            let inspected = backup_inspect(&output).unwrap();
            assert_eq!(inspected.warnings, saved.manifest.warnings);
            let data_dir = dir.path().join("restored");
            let restored = backup_restore(&RestoreConfig {
                snapshot_path: output,
                data_dir: data_dir.clone(),
                replace: true,
            })
            .unwrap();
            assert_eq!(restored.manifest.warnings, saved.manifest.warnings);
            let restored_db = data_dir.join("brain.lbug");
            let restored_activity = crate::sidecar_path(&restored_db, ".gitactivity.json");
            assert_eq!(restored_activity.exists(), !legacy);
            if !legacy {
                assert_eq!(
                    std::fs::read_to_string(&restored_activity).unwrap(),
                    payload
                );
            }
            let store = nestweaver_store::GraphStore::open(&restored_db).unwrap();
            // Query startup explicitly loads ranking sidecars after opening the graph.
            store.load_git_activity_sidecar(&restored_activity).unwrap();
            assert_eq!(
                store.git_activity_score("repo:test", "src/main.rs"),
                if legacy { None } else { Some(0.9) }
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn unreadable_git_activity_is_not_silently_omitted() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("brain.lbug");
        drop(nestweaver_store::GraphStore::open(&db).unwrap());
        let activity = crate::sidecar_path(&db, ".gitactivity.json");
        std::os::unix::fs::symlink(dir.path().join("missing-scores"), &activity).unwrap();
        let output = dir.path().join("backup.nwsnap.zst");
        assert!(
            backup_save(&BackupConfig {
                db_path: db,
                output_path: output.clone(),
                include_clones: false,
                instance_id: "test".into(),
                workspace_path: None,
                overwrite: false,
            })
            .is_err()
        );
        assert!(!output.exists());
        assert!(
            std::fs::symlink_metadata(activity)
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }

    #[test]
    fn invalid_git_activity_never_publishes_an_archive() {
        for payload in [
            "broken",
            r#"{"version":2}"#,
            r#"{"version":3}"#,
            r#"{"version":2,"repos":{"repo:test":"#,
            r#"{"version":null}"#,
            r#"{"version":3,"repos":{}}"#,
            r#"{"version":2,"repos":[]}"#,
            "null",
        ] {
            let dir = tempfile::tempdir().unwrap();
            let db = dir.path().join("brain.lbug");
            drop(nestweaver_store::GraphStore::open(&db).unwrap());
            let activity = crate::sidecar_path(&db, ".gitactivity.json");
            std::fs::write(&activity, payload).unwrap();
            let output = dir.path().join("backup.nwsnap.zst");
            assert!(
                backup_save(&BackupConfig {
                    db_path: db,
                    output_path: output.clone(),
                    include_clones: false,
                    instance_id: "test".into(),
                    workspace_path: None,
                    overwrite: false,
                })
                .is_err()
            );
            assert!(!output.exists());
            assert_eq!(std::fs::read_to_string(activity).unwrap(), payload);
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
            overwrite: false,
        };

        let result = backup_save(&config).unwrap();
        assert!(output.exists());
        assert_eq!(result.manifest.instance_id, "default");
        assert_eq!(result.manifest.tier, "standard");

        let manifest = backup_inspect(&output).unwrap();
        assert_eq!(manifest.instance_id, "default");
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
            overwrite: false,
        };

        backup_save(&config).unwrap();
        let manifest = backup_inspect(&output).unwrap();
        assert_eq!(manifest.instance_id, "default");
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
            overwrite: false,
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
            overwrite: false,
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

    /// nw-728. The first upgrade step is `backup save` of a database whose
    /// derived sidecars were written by the previous release. A producer-only
    /// mismatch is rebuildable; it must not refuse the archive. `--force` does
    /// not enter into it.
    #[test]
    fn backup_save_excludes_a_sidecar_from_an_older_producer() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::write(repo.join("a.py"), "def f():\n    return 1\n").unwrap();
        let db_path = dir.path().join("graph.lbug");
        crate::index::index_directory(&repo, &db_path, "test", "file:///repo", "sha").unwrap();
        let manifests = crate::manifest::manifest_cache_path(&db_path);
        let mut envelope: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&manifests).unwrap()).unwrap();
        envelope["producer_version"] = serde_json::Value::String("10.3.1".to_string());
        std::fs::write(&manifests, serde_json::to_vec_pretty(&envelope).unwrap()).unwrap();

        let output = dir.path().join("out.nwsnap.zst");
        let result = backup_save(&BackupConfig {
            db_path: db_path.clone(),
            output_path: output.clone(),
            include_clones: false,
            instance_id: "test".to_string(),
            workspace_path: None,
            overwrite: false,
        })
        .expect("an older producer on a rebuildable sidecar must not refuse backup");
        assert!(
            result
                .manifest
                .warnings
                .iter()
                .any(|warning| warning.contains("10.3.1")),
            "the operator must be told which sidecar was left out: {:?}",
            result.manifest.warnings
        );
        let inspected = backup_inspect(&output).unwrap();
        assert!(inspected.checksums.contains_key("graph.lbug"));
        assert!(
            !inspected
                .checksums
                .contains_key("graph.lbug.manifests.json")
        );
        assert!(
            manifests.exists(),
            "excluding a sidecar from the archive must not delete the live file"
        );
    }

    /// Widening the rebuildable exclusion must not ship a sidecar that belongs
    /// to a different graph.
    #[test]
    fn backup_save_still_refuses_a_foreign_artifact() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::write(repo.join("a.py"), "def f():\n    return 1\n").unwrap();
        let db_path = dir.path().join("graph.lbug");
        crate::index::index_directory(&repo, &db_path, "test", "file:///repo", "sha").unwrap();
        let manifests = crate::manifest::manifest_cache_path(&db_path);
        let before = std::fs::read(&manifests).unwrap();
        let mut envelope: serde_json::Value = serde_json::from_slice(&before).unwrap();
        envelope["brain_uuid"] =
            serde_json::Value::String("00000000-0000-4000-8000-000000000099".to_string());
        std::fs::write(&manifests, serde_json::to_vec_pretty(&envelope).unwrap()).unwrap();

        let output = dir.path().join("out.nwsnap.zst");
        let error = backup_save(&BackupConfig {
            db_path,
            output_path: output.clone(),
            include_clones: false,
            instance_id: "test".to_string(),
            workspace_path: None,
            overwrite: false,
        })
        .expect_err("a foreign artifact must still refuse the backup");
        let message = format!("{error:#}");
        assert!(message.contains("do not belong to this graph"), "{message}");
        assert!(message.contains("foreign artifact identity"), "{message}");
        assert!(
            !output.exists(),
            "a refused backup must not publish an archive"
        );
    }

    /// nw-727. Save must not truncate an archive that is already at the
    /// destination, and the bytes that land must be published by rename.
    #[test]
    fn backup_save_leaves_an_existing_archive_untouched_without_overwrite() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::write(repo.join("a.py"), "def f():\n    return 1\n").unwrap();
        let db_path = dir.path().join("graph.lbug");
        crate::index::index_directory(&repo, &db_path, "test", "file:///repo", "sha").unwrap();
        let output = dir.path().join("out.nwsnap.zst");
        std::fs::write(&output, b"keep-me").unwrap();

        let config = BackupConfig {
            db_path: db_path.clone(),
            output_path: output.clone(),
            include_clones: false,
            instance_id: "test".to_string(),
            workspace_path: None,
            overwrite: false,
        };
        let error = backup_save(&config).expect_err("an existing archive needs --force");
        let message = format!("{error:#}");
        assert!(message.contains("already exists"), "{message}");
        assert_eq!(std::fs::read(&output).unwrap(), b"keep-me");
        assert!(!leftover_partials(&output));

        let saved = backup_save(&BackupConfig {
            overwrite: true,
            ..config
        })
        .expect("overwrite publishes a new archive");
        assert_ne!(std::fs::read(&output).unwrap(), b"keep-me");
        assert_eq!(saved.output_path, output);
        assert!(backup_inspect(&output).is_ok());
        assert!(!leftover_partials(&output));
    }

    /// nw-727. Restore must not rename a non-empty data directory aside unless
    /// the caller asked to replace it.
    #[test]
    fn backup_restore_refuses_a_non_empty_data_dir_without_replace() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::write(repo.join("a.py"), "def f():\n    return 1\n").unwrap();
        let db_path = dir.path().join("graph.lbug");
        crate::index::index_directory(&repo, &db_path, "test", "file:///repo", "sha").unwrap();
        let snapshot = dir.path().join("out.nwsnap.zst");
        backup_save(&BackupConfig {
            db_path,
            output_path: snapshot.clone(),
            include_clones: false,
            instance_id: "test".to_string(),
            workspace_path: None,
            overwrite: false,
        })
        .unwrap();

        let data_dir = dir.path().join("live");
        std::fs::create_dir_all(&data_dir).unwrap();
        std::fs::write(data_dir.join("sentinel.txt"), b"keep").unwrap();
        // A NestWeaver data directory: `--force` replaces only one of those
        // (a directory with no database in it is refused even with --force).
        drop(nestweaver_store::GraphStore::create(&data_dir.join("prior.lbug")).unwrap());
        let error = backup_restore(&RestoreConfig {
            snapshot_path: snapshot.clone(),
            data_dir: data_dir.clone(),
            replace: false,
        })
        .expect_err("a non-empty data directory needs --force");
        let message = format!("{error:#}");
        assert!(message.contains("not empty"), "{message}");
        assert_eq!(
            std::fs::read(data_dir.join("sentinel.txt")).unwrap(),
            b"keep"
        );
        assert!(!data_dir.join("graph.lbug").exists());

        std::fs::remove_file(data_dir.join("sentinel.txt")).unwrap();
        backup_restore(&RestoreConfig {
            snapshot_path: snapshot,
            data_dir: data_dir.clone(),
            replace: true,
        })
        .expect("replace restores over the existing directory");
        assert!(data_dir.join("graph.lbug").exists());
        assert!(!data_dir.join("sentinel.txt").exists());
    }

    /// A retry of an interrupted restore reconciles the aside copy before it
    /// decides the target is occupied. The refusal must not claim that
    /// reconciliation left the directory untouched.
    #[test]
    fn restore_without_replace_reports_an_interrupted_restore_it_reconciled() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::write(repo.join("a.py"), "def f():\n    return 1\n").unwrap();
        let db_path = dir.path().join("graph.lbug");
        crate::index::index_directory(&repo, &db_path, "test", "file:///repo", "sha").unwrap();
        let snapshot = dir.path().join("out.nwsnap.zst");
        backup_save(&BackupConfig {
            db_path,
            output_path: snapshot.clone(),
            include_clones: false,
            instance_id: "test".to_string(),
            workspace_path: None,
            overwrite: false,
        })
        .unwrap();

        let data_dir = dir.path().join("live");
        let restoring = restoring_dir_for(&data_dir);
        std::fs::create_dir_all(&data_dir).unwrap();
        std::fs::create_dir_all(&restoring).unwrap();
        std::fs::write(data_dir.join("partial.txt"), b"partial").unwrap();
        std::fs::write(restoring.join("good.txt"), b"good").unwrap();
        // The pre-restore copy is a NestWeaver data directory: a later
        // `--force` only replaces one of those.
        drop(nestweaver_store::GraphStore::create(&restoring.join("prior.lbug")).unwrap());
        // A real restore takes its write lease before capturing the journal.
        // Simulate that crashed owner's stable operational lock before proof.
        drop(nestweaver_store::acquire_db_write_lease(&restoring.join("prior.lbug")).unwrap());
        publish_restore_phase(&RestoreJournal {
            version: RESTORE_JOURNAL_VERSION,
            disposition: Some(RestoreDisposition::DeleteProvenArchive),
            previous_preserved_copies: Vec::new(),
            phase: RestorePhase::CopyInProgress,
            data_dir: data_dir.clone(),
            restoring_dir: restoring.clone(),
            snapshot_path: snapshot.clone(),
            destination_identity: restore_directory_identity(&data_dir),
            restoring_identity: restore_directory_identity(&restoring),
            destination_paths: restore_tree_paths(&data_dir).unwrap(),
            restoring_paths: restore_tree_paths(&restoring).unwrap(),
            graph_path: None,
            source_checksums: None,
            restoring_file_witnesses: Some(restore_tree_witnesses(&restoring).unwrap()),
            destination_file_witnesses: None,
            legacy_engine: false,
        })
        .unwrap();

        let error = backup_restore(&RestoreConfig {
            snapshot_path: snapshot.clone(),
            data_dir: data_dir.clone(),
            replace: false,
        })
        .expect_err("a reconciled directory still needs --force");
        let message = format!("{error:#}");
        assert!(
            message.contains("interrupted restore was reconciled"),
            "{message}"
        );
        assert!(!message.contains("Nothing in"), "{message}");
        assert_eq!(std::fs::read(data_dir.join("good.txt")).unwrap(), b"good");
        assert!(!data_dir.join("partial.txt").exists());
        assert!(!restoring.exists());
        assert!(!restore_journal_path(&data_dir).exists());

        std::fs::remove_file(data_dir.join("good.txt")).unwrap();
        backup_restore(&RestoreConfig {
            snapshot_path: snapshot,
            data_dir: data_dir.clone(),
            replace: true,
        })
        .expect("force applies the archive after reconciliation");
        assert!(data_dir.join("graph.lbug").is_file());
        assert!(!data_dir.join("good.txt").exists());
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
    fn backup_waits_for_an_idle_connection_then_packages_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("overlap.lbug");
        let store = std::sync::Arc::new(nestweaver_store::GraphStore::create(&db_path).unwrap());
        let config = BackupConfig {
            db_path,
            output_path: dir.path().join("overlap.nwsnap.zst"),
            include_clones: false,
            instance_id: "default".into(),
            workspace_path: None,
            overwrite: false,
        };
        // An idle connection counts too, just like a daemon background read
        // between its final native query and connection destruction.
        let reader = store.begin_transaction().unwrap();
        store.commit_transaction(&reader).unwrap();
        let backup_store = std::sync::Arc::clone(&store);
        let backup_config = config.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            tx.send(stage_backup_from_store(&backup_store, &backup_config))
                .unwrap();
        });
        let early = rx.recv_timeout(Duration::from_millis(100));
        drop(reader);
        let staged = match early {
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                rx.recv_timeout(Duration::from_secs(5)).unwrap()
            }
            other => other.unwrap(),
        }
        .expect("backup must tolerate the temporary idle connection");
        worker.join().unwrap();
        package_staged(&config, staged).unwrap();
        assert_eq!(backup_inspect(&config.output_path).unwrap().symbol_count, 0);
        assert!(store.count_symbols().is_ok());
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
            overwrite: false,
        };

        let staged = stage_backup_from_store(&store, &config).expect("stage");
        let result = package_staged(&config, staged).expect("package");
        // Store is still open here — the daemon keeps serving.
        assert!(store.count_symbols().is_ok());
        assert!(output.exists());
        assert_eq!(result.manifest.instance_id, "default");
        assert_eq!(backup_inspect(&output).unwrap().instance_id, "default");
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
            overwrite: false,
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
            overwrite: false,
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
            overwrite: false,
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
            overwrite: false,
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
            replace: true,
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
            overwrite: false,
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
    fn unjournaled_leftover_restoring_dir_is_preserved_without_promoting_it() {
        let tmp = tempfile::TempDir::new().unwrap();
        let data = tmp.path().join("data");
        let restoring = tmp.path().join("data.restoring");
        std::fs::create_dir_all(&restoring).unwrap();
        std::fs::write(restoring.join("brain.lbug"), b"RECOVERED").unwrap();
        let recovery = recover_interrupted_restore(&data).unwrap();
        let preserved = recovery
            .preserved
            .expect("unjournaled bytes cannot be granted implicit restore ownership");
        assert!(!data.exists());
        assert!(!restoring.exists());
        assert_eq!(
            std::fs::read(preserved.join("brain.lbug")).unwrap(),
            b"RECOVERED"
        );
    }

    // -----------------------------------------------------------------------
    // Restore recovery: no failure path may delete the last complete copy
    // -----------------------------------------------------------------------

    /// A small, structurally complete stand-in dataset carrying an identifying
    /// marker, so a test can say WHICH copy survived rather than merely that
    /// something did.
    fn seed_dataset(dir: &Path, marker: &[u8]) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join("brain.lbug"), marker).unwrap();
        std::fs::write(dir.join("brain.lbug.filemeta.json"), b"{}").unwrap();
    }

    /// Every directory directly under `root` holding a `brain.lbug` equal to
    /// `marker`.
    ///
    /// The assertions use this instead of one hard-coded path because a
    /// correct recovery is allowed to MOVE the surviving copy — what it is
    /// never allowed to do is destroy it.
    fn copies_of(root: &Path, marker: &[u8]) -> Vec<PathBuf> {
        let mut found: Vec<PathBuf> = std::fs::read_dir(root)
            .unwrap()
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| std::fs::read(path.join("brain.lbug")).ok().as_deref() == Some(marker))
            .collect();
        found.sort();
        found
    }

    fn journal_at(data_dir: &Path, phase: RestorePhase) {
        publish_restore_phase(&RestoreJournal {
            version: RESTORE_JOURNAL_VERSION,
            disposition: Some(RestoreDisposition::DeleteProvenArchive),
            previous_preserved_copies: Vec::new(),
            phase,
            data_dir: data_dir.to_path_buf(),
            restoring_dir: restoring_dir_for(data_dir),
            snapshot_path: PathBuf::from("/nonexistent.nwsnap.zst"),
            destination_identity: restore_directory_identity(data_dir),
            restoring_identity: restore_directory_identity(&restoring_dir_for(data_dir)),
            destination_paths: restore_tree_paths(data_dir).unwrap(),
            restoring_paths: restore_tree_paths(&restoring_dir_for(data_dir)).unwrap(),
            graph_path: None,
            source_checksums: None,
            restoring_file_witnesses: Some(
                restore_tree_witnesses(&restoring_dir_for(data_dir)).unwrap(),
            ),
            destination_file_witnesses: None,
            legacy_engine: false,
        })
        .unwrap();
    }

    #[test]
    fn stale_or_copied_journal_cannot_delete_a_reused_foreign_target() {
        let tmp = tempfile::tempdir().unwrap();
        let data = tmp.path().join("data");
        seed_dataset(&data, b"PARTIAL");
        journal_at(&data, RestorePhase::CopyInProgress);
        let original = tmp.path().join("original");
        std::fs::rename(&data, &original).unwrap();
        std::fs::create_dir(&data).unwrap();
        std::fs::write(data.join("thesis.txt"), b"mine").unwrap();
        assert!(recover_interrupted_restore(&data).is_err());
        assert_eq!(std::fs::read(data.join("thesis.txt")).unwrap(), b"mine");
        let copied = tmp.path().join("copied");
        std::fs::create_dir(&copied).unwrap();
        std::fs::write(copied.join("thesis.txt"), b"mine too").unwrap();
        std::fs::copy(restore_journal_path(&data), restore_journal_path(&copied)).unwrap();
        recover_interrupted_restore(&copied).unwrap();
        assert_eq!(
            std::fs::read(copied.join("thesis.txt")).unwrap(),
            b"mine too"
        );
    }

    #[test]
    fn journal_does_not_authorize_unknown_files_added_after_it_was_published() {
        let tmp = tempfile::tempdir().unwrap();
        let data = tmp.path().join("data");
        let restoring = restoring_dir_for(&data);
        seed_dataset(&data, b"RESTORED");
        seed_dataset(&restoring, b"SUPERSEDED");
        journal_at(&data, RestorePhase::Cleanup);
        std::fs::write(restoring.join("thesis.txt"), b"mine").unwrap();
        let before_journal = std::fs::read(restore_journal_path(&data)).unwrap();
        assert!(recover_interrupted_restore(&data).is_err());
        assert_eq!(
            std::fs::read(restoring.join("thesis.txt")).unwrap(),
            b"mine"
        );
        assert_eq!(
            std::fs::read(restoring.join("brain.lbug")).unwrap(),
            b"SUPERSEDED"
        );
        assert_eq!(std::fs::read(data.join("brain.lbug")).unwrap(), b"RESTORED");
        assert_eq!(
            std::fs::read(restore_journal_path(&data)).unwrap(),
            before_journal
        );
    }

    /// A real archived/restored graph, including its original copied marker.
    fn seed_marker_replay_dataset(root: &Path, graph: &str) -> (PathBuf, PathBuf, Vec<u8>) {
        let source = root.join("source");
        std::fs::create_dir_all(&source).unwrap();
        let db = source.join(graph);
        let store = nestweaver_store::GraphStore::create(&db).unwrap();
        store.ensure_data_instance_id("default").unwrap();
        drop(store);
        let snapshot = root.join("source.nwsnap.zst");
        backup_save(&BackupConfig {
            db_path: db.clone(),
            output_path: snapshot.clone(),
            include_clones: false,
            instance_id: "default".into(),
            workspace_path: None,
            overwrite: false,
        })
        .unwrap();
        let original_marker =
            std::fs::read(nestweaver_store::engine_format::sidecar_path(&db)).unwrap();
        let data = root.join("data");
        backup_restore(&RestoreConfig {
            snapshot_path: snapshot.clone(),
            data_dir: data.clone(),
            replace: true,
        })
        .unwrap();
        (data, snapshot, original_marker)
    }

    fn marker_replay_journal(data: &Path, snapshot: &Path, graph: &str, phase: RestorePhase) {
        journal_at(data, phase);
        let mut journal = read_restore_journal(data).unwrap();
        journal.snapshot_path = snapshot.to_path_buf();
        journal.graph_path = Some(graph.into());
        journal.source_checksums = Some(backup_inspect(snapshot).unwrap().checksums);
        journal.destination_file_witnesses = Some(restore_tree_witnesses(data).unwrap());
        publish_restore_phase(&journal).unwrap();
    }

    fn generate_live_restore_artifacts(data: &Path) {
        let db = data.join("brain.lbug");
        drop(
            crate::publication::PublicationRootLock::acquire(
                &crate::publication::default_publication_root(&db),
            )
            .unwrap(),
        );
        let search =
            nestweaver_store::TantivyIndex::open_or_create(&crate::sidecar_path(&db, ".tantivy"))
                .unwrap();
        search
            .update_note(
                "note:live",
                "Live retained note",
                "vault:live",
                &["live preserved bytes".into()],
                &[],
                &[],
                &[],
            )
            .unwrap();
        drop(search);
        assert_eq!(
            replacement_disposition(data).unwrap(),
            ReplacementDisposition::PreserveLive
        );
    }

    fn tree_bytes(root: &Path) -> std::collections::BTreeMap<String, Vec<u8>> {
        walkdir::WalkDir::new(root)
            .into_iter()
            .map(Result::unwrap)
            .filter(|entry| entry.file_type().is_file())
            .map(|entry| {
                (
                    normalized_relative_path(root, entry.path()).unwrap(),
                    std::fs::read(entry.path()).unwrap(),
                )
            })
            .collect()
    }

    #[test]
    fn live_repeat_restore_preserves_every_original_byte_and_discloses_all_copies() {
        let tmp = tempfile::tempdir().unwrap();
        let (data, archive, _) = seed_marker_replay_dataset(tmp.path(), "brain.lbug");
        for _ in 0..2 {
            generate_live_restore_artifacts(&data);
            let before = tree_bytes(&data);
            let restored = backup_restore(&RestoreConfig {
                snapshot_path: archive.clone(),
                data_dir: data.clone(),
                replace: true,
            })
            .unwrap();
            assert_eq!(restored.preserved_copies.len(), 1);
            assert_eq!(
                restored.preserved_copy.as_ref(),
                restored.preserved_copies.first()
            );
            let retained = &restored.preserved_copies[0];
            assert_eq!(tree_bytes(retained), before);
            assert_ne!(retained, &data);
            assert_ne!(retained, &restoring_dir_for(&data));
            assert!(
                nestweaver_store::engine_format::has_current_file_marker(&data.join("brain.lbug"))
                    .unwrap()
            );
        }
    }

    #[test]
    fn public_retry_completes_live_preservation_at_each_real_crash_boundary() {
        for copy in [false, true] {
            for phase in [
                "before-live-preserve",
                "after-live-preserve",
                "live-preserve-durable",
            ] {
                let tmp = tempfile::tempdir().unwrap();
                let (data, archive, _) = seed_marker_replay_dataset(tmp.path(), "brain.lbug");
                generate_live_restore_artifacts(&data);
                let before = tree_bytes(&data);
                let output = run_restore_in_child_copy_mode(&archive, &data, phase, &[], copy);
                assert_eq!(
                    output.status.code(),
                    Some(9),
                    "{phase}: {}",
                    String::from_utf8_lossy(&output.stderr)
                );
                let journal = read_restore_journal(&data).unwrap();
                assert!(journal.restoring_file_witnesses.is_none());
                assert!(journal.restoring_identity.is_none());
                assert!(journal.restoring_paths.is_empty());
                let expected = match journal.disposition.unwrap() {
                    RestoreDisposition::PreserveLive { receipt } => receipt.preserved_path,
                    _ => panic!("preserve receipt missing"),
                };
                let result = backup_restore(&RestoreConfig {
                    snapshot_path: archive,
                    data_dir: data.clone(),
                    replace: true,
                })
                .unwrap();
                assert!(result.preserved_copies.contains(&expected));
                assert_eq!(tree_bytes(&expected), before);
                assert!(!restore_journal_path(&data).exists());
                assert!(!restoring_dir_for(&data).exists());
            }
        }
    }

    #[test]
    fn live_preservation_version_tag_loss_never_creates_legacy_delete_authority() {
        for change in [
            "missing-tag",
            "unknown-tag",
            "version-only",
            "version-and-entire-tag",
            "missing-reservation",
            "future-version",
        ] {
            let tmp = tempfile::tempdir().unwrap();
            let (data, archive, _) = seed_marker_replay_dataset(tmp.path(), "brain.lbug");
            generate_live_restore_artifacts(&data);
            let output =
                run_restore_in_child_copy_mode(&archive, &data, "cutover-validated", &[], false);
            assert_eq!(output.status.code(), Some(9));
            let path = restore_journal_path(&data);
            let mut value: serde_json::Value =
                serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
            assert!(value["restoring_file_witnesses"].is_null());
            match change {
                "missing-tag" => {
                    value.as_object_mut().unwrap().remove("disposition");
                }
                "unknown-tag" => value["disposition"]["policy"] = "Unknown".into(),
                "version-only" => value["version"] = 3.into(),
                "future-version" => value["version"] = 5.into(),
                "version-and-entire-tag" => {
                    value["version"] = 3.into();
                    value.as_object_mut().unwrap().remove("disposition");
                }
                "missing-reservation" => {
                    value["disposition"]["receipt"]
                        .as_object_mut()
                        .unwrap()
                        .remove("reservation_identity");
                }
                _ => unreachable!(),
            }
            std::fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
            let before = tree_bytes(&data);
            let aside = tree_bytes(&restoring_dir_for(&data));
            let journal = std::fs::read(&path).unwrap();
            for _ in 0..2 {
                let error = recover_interrupted_restore(&data).unwrap_err();
                assert!(
                    error.downcast_ref::<BackupRefusal>().is_some(),
                    "{change}: {error:#}"
                );
                assert_eq!(tree_bytes(&data), before);
                assert_eq!(tree_bytes(&restoring_dir_for(&data)), aside);
                assert_eq!(std::fs::read(&path).unwrap(), journal);
            }
        }
    }

    #[test]
    fn live_preservation_reserved_child_and_parent_exchange_never_overwrite_content() {
        for step in [
            "occupied-child",
            "racing-child",
            "before-rename",
            "after-rename",
        ] {
            let tmp = tempfile::tempdir().unwrap();
            let (data, archive, _) = seed_marker_replay_dataset(tmp.path(), "brain.lbug");
            generate_live_restore_artifacts(&data);
            let before = tree_bytes(&data);
            let output =
                run_restore_in_child_copy_mode(&archive, &data, "cutover-validated", &[], false);
            assert_eq!(output.status.code(), Some(9));
            let journal = read_restore_journal(&data).unwrap();
            let RestoreDisposition::PreserveLive { receipt } =
                journal.disposition.as_ref().unwrap()
            else {
                panic!()
            };
            let saved = tmp.path().join("reserved-parent-retained");
            if step == "occupied-child" {
                std::fs::write(&receipt.preserved_path, b"foreign child").unwrap();
            }
            let error = receipt
                .finish_with_hook(&data, |at| {
                    if step == "racing-child" && at == "before-rename" {
                        std::fs::write(&receipt.preserved_path, b"foreign child")?;
                    }
                    if at == step {
                        std::fs::rename(&receipt.reservation, &saved)?;
                        std::fs::create_dir(&receipt.reservation)?;
                        std::fs::write(receipt.reservation.join("personal"), b"foreign parent")?;
                    }
                    Ok(())
                })
                .unwrap_err();
            assert!(
                error.downcast_ref::<BackupRefusal>().is_some(),
                "{step}: {error:#}"
            );
            assert!(restore_journal_path(&data).exists());
            if step == "after-rename" {
                assert_eq!(tree_bytes(&saved.join("original")), before);
                assert!(format!("{error:#}").contains(&saved.display().to_string()));
            } else {
                assert_eq!(tree_bytes(&restoring_dir_for(&data)), before);
            }
            if ["occupied-child", "racing-child"].contains(&step) {
                assert_eq!(
                    std::fs::read(&receipt.preserved_path).unwrap(),
                    b"foreign child"
                );
            } else {
                assert_eq!(
                    std::fs::read(receipt.reservation.join("personal")).unwrap(),
                    b"foreign parent"
                );
            }
        }
    }

    #[test]
    fn actual_legacy_v3_strong_proof_cleanup_remains_supported() {
        let tmp = tempfile::tempdir().unwrap();
        let (data, archive, _) = seed_marker_replay_dataset(tmp.path(), "brain.lbug");
        let output =
            run_restore_in_child_copy_mode(&archive, &data, "cutover-validated", &[], false);
        assert_eq!(output.status.code(), Some(9));
        use std::os::unix::fs::MetadataExt;
        let old_witnesses = |root: &Path| {
            walkdir::WalkDir::new(root).into_iter().map(Result::unwrap)
                .filter(|entry| entry.file_type().is_file()).map(|entry| {
                    let meta = entry.metadata().unwrap();
                    (normalized_relative_path(root, entry.path()).unwrap(), serde_json::json!({
                        "device":meta.dev(), "inode":meta.ino(), "length":meta.len(),
                        "modified_seconds":meta.mtime(), "modified_nanoseconds":meta.mtime_nsec(),
                        "changed_seconds":meta.ctime(), "changed_nanoseconds":meta.ctime_nsec()
                    }))
                }).collect::<serde_json::Map<String, serde_json::Value>>()
        };
        let aside = restoring_dir_for(&data);
        let wire = serde_json::json!({
            "version":3, "phase":"cutover-validated", "data_dir":data,
            "restoring_dir":aside, "snapshot_path":archive,
            "destination_identity":restore_directory_identity(&data),
            "restoring_identity":restore_directory_identity(&aside),
            "destination_paths":restore_tree_paths(&data).unwrap(),
            "restoring_paths":restore_tree_paths(&aside).unwrap(),
            "graph_path":"brain.lbug", "source_checksums":backup_inspect(&archive).unwrap().checksums,
            "restoring_file_witnesses":old_witnesses(&aside),
            "destination_file_witnesses":old_witnesses(&data), "legacy_engine":false
        });
        assert!(!wire.as_object().unwrap().contains_key("disposition"));
        std::fs::write(
            restore_journal_path(&data),
            serde_json::to_vec(&wire).unwrap(),
        )
        .unwrap();
        let recovered = recover_interrupted_restore(&data).unwrap();
        assert!(recovered.preserved.is_none());
        assert!(!restoring_dir_for(&data).exists());
        assert!(!restore_journal_path(&data).exists());
    }

    #[test]
    fn live_unknown_nested_and_nonempty_locks_remain_refused_without_mutation() {
        for bad in [
            "brain.lbug.publications/personal.txt",
            "brain.lbug.tantivy/nested/private.txt",
            "brain.lbug.publications/LOCK",
        ] {
            let tmp = tempfile::tempdir().unwrap();
            let (data, archive, _) = seed_marker_replay_dataset(tmp.path(), "brain.lbug");
            generate_live_restore_artifacts(&data);
            let path = data.join(bad);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, b"private bytes").unwrap();
            let before = tree_bytes(&data);
            assert!(
                backup_restore(&RestoreConfig {
                    snapshot_path: archive,
                    data_dir: data.clone(),
                    replace: true
                })
                .is_err()
            );
            assert_eq!(tree_bytes(&data), before);
        }
    }

    #[test]
    fn earlier_foreign_aside_and_current_live_copy_are_both_disclosed_after_crash() {
        for missing_archive in [false, true] {
            let tmp = tempfile::tempdir().unwrap();
            let (data, archive, _) = seed_marker_replay_dataset(tmp.path(), "brain.lbug");
            generate_live_restore_artifacts(&data);
            let current = tree_bytes(&data);
            let seed = tmp.path().join("foreign-seed");
            backup_restore(&RestoreConfig {
                snapshot_path: archive.clone(),
                data_dir: seed.clone(),
                replace: true,
            })
            .unwrap();
            let foreign = tree_bytes(&seed);
            std::fs::rename(&seed, restoring_dir_for(&data)).unwrap();
            let output =
                run_restore_in_child_copy_mode(&archive, &data, "after-live-preserve", &[], false);
            assert_eq!(output.status.code(), Some(9));
            let journal = read_restore_journal(&data).unwrap();
            assert_eq!(journal.previous_preserved_copies.len(), 1);
            let mut expected = journal.previous_preserved_copies;
            let RestoreDisposition::PreserveLive { receipt } = journal.disposition.unwrap() else {
                panic!()
            };
            expected.push(receipt.preserved_path);
            if missing_archive {
                std::fs::remove_file(&archive).unwrap();
            }
            let result = backup_restore(&RestoreConfig {
                snapshot_path: archive,
                data_dir: data.clone(),
                replace: true,
            });
            if missing_archive {
                let error = format!("{:#}", result.unwrap_err());
                for path in &expected {
                    assert!(error.contains(&path.display().to_string()), "{error}");
                }
            } else {
                let result = result.unwrap();
                assert_eq!(result.preserved_copies, expected);
                assert_eq!(
                    result.preserved_copy.as_ref(),
                    result.preserved_copies.first()
                );
            }
            assert_eq!(tree_bytes(&expected[0]), foreign);
            assert_eq!(tree_bytes(&expected[1]), current);
        }
    }

    #[test]
    fn missing_archive_after_live_replay_discloses_the_original_retained_path() {
        let tmp = tempfile::tempdir().unwrap();
        let (data, archive, _) = seed_marker_replay_dataset(tmp.path(), "brain.lbug");
        generate_live_restore_artifacts(&data);
        let before = tree_bytes(&data);
        let output =
            run_restore_in_child_copy_mode(&archive, &data, "after-live-preserve", &[], false);
        assert_eq!(output.status.code(), Some(9));
        let journal = read_restore_journal(&data).unwrap();
        let RestoreDisposition::PreserveLive { receipt } = journal.disposition.unwrap() else {
            panic!()
        };
        std::fs::remove_file(&archive).unwrap();
        let error = backup_restore(&RestoreConfig {
            snapshot_path: archive,
            data_dir: data.clone(),
            replace: true,
        })
        .unwrap_err();
        assert!(format!("{error:#}").contains(&receipt.preserved_path.display().to_string()));
        assert_eq!(tree_bytes(&receipt.preserved_path), before);
        assert!(
            nestweaver_store::engine_format::has_current_file_marker(&data.join("brain.lbug"))
                .unwrap()
        );
    }

    #[test]
    fn preservation_replay_refuses_changed_original_or_reservation_proof() {
        for change in [
            "changed-file",
            "missing-reservation",
            "foreign-child",
            "both-copies",
        ] {
            let tmp = tempfile::tempdir().unwrap();
            let (data, archive, _) = seed_marker_replay_dataset(tmp.path(), "brain.lbug");
            generate_live_restore_artifacts(&data);
            let output =
                run_restore_in_child_copy_mode(&archive, &data, "cutover-validated", &[], false);
            assert_eq!(output.status.code(), Some(9));
            let journal = read_restore_journal(&data).unwrap();
            let RestoreDisposition::PreserveLive { receipt } = journal.disposition.unwrap() else {
                panic!()
            };
            let aside = restoring_dir_for(&data);
            match change {
                "changed-file" => {
                    std::fs::write(aside.join("brain.lbug.publications/LOCK"), b"changed").unwrap()
                }
                "missing-reservation" => {
                    std::fs::rename(
                        &receipt.reservation,
                        tmp.path().join("reservation-retained"),
                    )
                    .unwrap();
                }
                "foreign-child" => {
                    std::fs::create_dir(&receipt.preserved_path).unwrap();
                    std::fs::write(receipt.preserved_path.join("foreign"), b"keep").unwrap();
                }
                "both-copies" => {
                    std::fs::create_dir(&receipt.preserved_path).unwrap();
                }
                _ => unreachable!(),
            }
            let data_before = tree_bytes(&data);
            let aside_before = tree_bytes(&aside);
            let journal_before = std::fs::read(restore_journal_path(&data)).unwrap();
            for _ in 0..2 {
                let error = backup_restore(&RestoreConfig {
                    snapshot_path: archive.clone(),
                    data_dir: data.clone(),
                    replace: true,
                })
                .unwrap_err();
                assert!(error.downcast_ref::<BackupRefusal>().is_some(), "{error:#}");
                assert_eq!(tree_bytes(&data), data_before);
                assert_eq!(tree_bytes(&aside), aside_before);
                assert_eq!(
                    std::fs::read(restore_journal_path(&data)).unwrap(),
                    journal_before
                );
            }
        }
    }

    #[test]
    fn malformed_live_markdown_skipped_and_catalog_descriptors_never_gain_admission() {
        for suffix in [
            ".markdown-derivation.json",
            ".skipped_notes.json",
            ".tantivy/meta.json",
            ".tantivy/.managed.json",
        ] {
            let tmp = tempfile::tempdir().unwrap();
            let (data, archive, _) = seed_marker_replay_dataset(tmp.path(), "brain.lbug");
            generate_live_restore_artifacts(&data);
            std::fs::write(
                data.join(format!("brain.lbug{suffix}")),
                br#"{"schema_version":9999,"version":9999,"payload":{}}"#,
            )
            .unwrap();
            let before = tree_bytes(&data);
            assert!(
                backup_restore(&RestoreConfig {
                    snapshot_path: archive,
                    data_dir: data.clone(),
                    replace: true
                })
                .is_err()
            );
            assert_eq!(tree_bytes(&data), before);
        }
    }

    #[test]
    fn copied_live_catalog_can_only_retain_whole_incumbent_and_never_override_physical_identity() {
        for copied_marker in [false, true] {
            let tmp = tempfile::tempdir().unwrap();
            let (data, archive, _) = seed_marker_replay_dataset(tmp.path(), "brain.lbug");
            generate_live_restore_artifacts(&data);
            let foreign = tmp.path().join("foreign-search");
            let search = nestweaver_store::TantivyIndex::open_or_create(&foreign).unwrap();
            search
                .update_note(
                    "foreign:note",
                    "Foreign retained witness",
                    "foreign:vault",
                    &["DO_NOT_DELETE_FOREIGN".into()],
                    &[],
                    &[],
                    &[],
                )
                .unwrap();
            drop(search);
            let target_search = crate::sidecar_path(&data.join("brain.lbug"), ".tantivy");
            std::fs::remove_dir_all(&target_search).unwrap();
            std::fs::create_dir(&target_search).unwrap();
            for entry in std::fs::read_dir(&foreign).unwrap() {
                let entry = entry.unwrap();
                std::fs::copy(entry.path(), target_search.join(entry.file_name())).unwrap();
            }
            if copied_marker {
                let unrelated = tmp.path().join("unrelated.lbug");
                drop(nestweaver_store::GraphStore::create(&unrelated).unwrap());
                std::fs::copy(
                    nestweaver_store::engine_format::sidecar_path(&unrelated),
                    nestweaver_store::engine_format::sidecar_path(&data.join("brain.lbug")),
                )
                .unwrap();
            }
            let before = tree_bytes(&data);
            let result = backup_restore(&RestoreConfig {
                snapshot_path: archive,
                data_dir: data.clone(),
                replace: true,
            });
            if copied_marker {
                assert!(result.is_err());
                assert_eq!(tree_bytes(&data), before);
            } else {
                let retained = result.unwrap().preserved_copy.unwrap();
                assert_eq!(tree_bytes(&retained), before);
                assert_eq!(
                    tree_bytes(&retained.join("brain.lbug.tantivy")),
                    tree_bytes(&foreign)
                );
            }
        }
    }

    #[test]
    fn live_unvalidated_rollback_retires_only_its_exact_empty_reservation() {
        for phase in ["staging-verified", "copy-midway", "old-data-preserved"] {
            let tmp = tempfile::tempdir().unwrap();
            let (data, archive, _) = seed_marker_replay_dataset(tmp.path(), "brain.lbug");
            generate_live_restore_artifacts(&data);
            let before = tree_bytes(&data);
            let output = run_restore_in_child_copy_mode(&archive, &data, phase, &[], true);
            assert_eq!(output.status.code(), Some(9), "{phase}");
            let journal = read_restore_journal(&data).unwrap();
            let RestoreDisposition::PreserveLive { receipt } = journal.disposition.unwrap() else {
                panic!()
            };
            assert!(receipt.reservation.is_dir());
            std::fs::remove_file(&archive).unwrap();
            let error = backup_restore(&RestoreConfig {
                snapshot_path: archive,
                data_dir: data.clone(),
                replace: true,
            })
            .unwrap_err();
            assert_eq!(tree_bytes(&data), before, "{phase}: {error:#}");
            assert!(!receipt.reservation.exists());
            assert!(!restore_journal_path(&data).exists());
        }
    }

    #[test]
    fn live_preservation_source_parent_exchange_before_and_after_rename_retains_all_bytes() {
        for step in ["before-rename", "after-rename"] {
            let tmp = tempfile::tempdir().unwrap();
            let namespace = tmp.path().join("namespace");
            std::fs::create_dir(&namespace).unwrap();
            let (data, archive, _) = seed_marker_replay_dataset(&namespace, "brain.lbug");
            generate_live_restore_artifacts(&data);
            let old = tree_bytes(&data);
            let output =
                run_restore_in_child_copy_mode(&archive, &data, "cutover-validated", &[], false);
            assert_eq!(output.status.code(), Some(9));
            let destination = tree_bytes(&data);
            let journal = read_restore_journal(&data).unwrap();
            let journal_bytes = std::fs::read(restore_journal_path(&data)).unwrap();
            let RestoreDisposition::PreserveLive { receipt } = journal.disposition.unwrap() else {
                panic!()
            };
            let saved = tmp.path().join("namespace-retained");
            let error = receipt
                .finish_with_hook(&data, |at| {
                    if at == step {
                        std::fs::rename(&namespace, &saved)?;
                        std::fs::create_dir(&namespace)?;
                        std::fs::write(namespace.join("foreign"), b"foreign namespace")?;
                    }
                    Ok(())
                })
                .unwrap_err();
            assert!(error.downcast_ref::<BackupRefusal>().is_some(), "{error:#}");
            assert_eq!(tree_bytes(&saved.join("data")), destination);
            let original_location = if step == "after-rename" {
                saved
                    .join(receipt.reservation.file_name().unwrap())
                    .join("original")
            } else {
                restoring_dir_for(&saved.join("data"))
            };
            assert_eq!(tree_bytes(&original_location), old);
            assert_eq!(
                std::fs::read(restore_journal_path(&saved.join("data"))).unwrap(),
                journal_bytes
            );
            assert_eq!(
                std::fs::read(namespace.join("foreign")).unwrap(),
                b"foreign namespace"
            );
            assert!(format!("{error:#}").contains(&saved.display().to_string()));
        }
    }

    #[test]
    fn live_preservation_io_failures_keep_copy_locations_and_operational_classification() {
        for fault in [
            "mkdir",
            "journal-publish",
            "rename",
            "reservation-fsync",
            "parent-fsync",
            "journal-retire",
            "journal-retire-fsync",
        ] {
            let tmp = tempfile::tempdir().unwrap();
            let (data, archive, _) = seed_marker_replay_dataset(tmp.path(), "brain.lbug");
            generate_live_restore_artifacts(&data);
            let before = tree_bytes(&data);
            let output = run_restore_in_child_copy_mode(
                &archive,
                &data,
                "unused",
                &[("NW_RESTORE_TEST_LIVE_IO_FAIL", fault)],
                false,
            );
            assert_eq!(output.status.code(), Some(3), "{fault}");
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert!(
                stderr.contains("typed_backup_refusal=false"),
                "{fault}: {stderr}"
            );
            assert!(stderr.contains(&format!("injected live restore {fault} I/O failure")));
            let reservation = std::fs::read_dir(tmp.path())
                .unwrap()
                .map(Result::unwrap)
                .map(|entry| entry.path())
                .find(|path| {
                    path.file_name()
                        .unwrap()
                        .to_string_lossy()
                        .starts_with("data.preserved-")
                });
            match fault {
                "mkdir" => {
                    assert!(reservation.is_none());
                    assert_eq!(tree_bytes(&data), before);
                }
                "journal-publish" => {
                    let reservation = reservation.unwrap();
                    assert!(std::fs::read_dir(&reservation).unwrap().next().is_none());
                    assert!(stderr.contains(&reservation.display().to_string()));
                    assert_eq!(tree_bytes(&data), before);
                }
                "rename" => {
                    let reservation = reservation.unwrap();
                    assert!(std::fs::read_dir(&reservation).unwrap().next().is_none());
                    assert_eq!(tree_bytes(&restoring_dir_for(&data)), before);
                    assert!(stderr.contains(&reservation.display().to_string()));
                    assert!(stderr.contains(&reservation.join("original").display().to_string()));
                }
                _ => {
                    let retained = reservation.unwrap().join("original");
                    assert_eq!(tree_bytes(&retained), before);
                    assert!(stderr.contains(&retained.display().to_string()), "{stderr}");
                    assert!(!restoring_dir_for(&data).exists());
                }
            }
            let journal_expected =
                !["mkdir", "journal-publish", "journal-retire-fsync"].contains(&fault);
            assert_eq!(
                restore_journal_path(&data).exists(),
                journal_expected,
                "{fault}: {stderr}"
            );
            if fault == "journal-retire-fsync" {
                assert!(!stderr.contains("journal retained"), "{stderr}");
            }
        }
    }

    #[test]
    fn public_replay_refusal_reports_current_reservation_and_every_prior_copy() {
        for has_prior in [false, true] {
            let tmp = tempfile::tempdir().unwrap();
            let (data, archive, _) = seed_marker_replay_dataset(tmp.path(), "brain.lbug");
            generate_live_restore_artifacts(&data);
            let output =
                run_restore_in_child_copy_mode(&archive, &data, "cutover-validated", &[], false);
            assert_eq!(output.status.code(), Some(9));
            let mut journal = read_restore_journal(&data).unwrap();
            let prior = tmp.path().join("prior-partial-copy");
            if has_prior {
                std::fs::create_dir(&prior).unwrap();
                std::fs::write(prior.join("partial"), b"incomplete retained bytes").unwrap();
                journal.previous_preserved_copies.push(prior.clone());
                publish_restore_phase(&journal).unwrap();
            }
            let RestoreDisposition::PreserveLive { receipt } = journal.disposition.unwrap() else {
                panic!()
            };
            std::fs::write(&receipt.preserved_path, b"foreign child").unwrap();
            let before = tree_bytes(tmp.path());
            for _ in 0..2 {
                let error = backup_restore(&RestoreConfig {
                    snapshot_path: archive.clone(),
                    data_dir: data.clone(),
                    replace: true,
                })
                .unwrap_err();
                assert!(error.downcast_ref::<BackupRefusal>().is_some(), "{error:#}");
                let message = format!("{error:#}");
                for path in [
                    &receipt.reservation,
                    &receipt.preserved_path,
                    &data,
                    &restoring_dir_for(&data),
                ] {
                    assert!(message.contains(&path.display().to_string()), "{message}");
                }
                assert!(message.contains("intended retained dataset"));
                assert!(!message.contains("complete pre-restore"));
                if has_prior {
                    assert!(message.contains(&prior.display().to_string()));
                }
                assert_eq!(tree_bytes(tmp.path()), before);
            }
        }
    }

    #[test]
    fn live_replay_io_errors_keep_current_and_prior_locations_without_becoming_refusals() {
        for fault in ["rename", "reservation-fsync"] {
            let tmp = tempfile::tempdir().unwrap();
            let (data, archive, _) = seed_marker_replay_dataset(tmp.path(), "brain.lbug");
            generate_live_restore_artifacts(&data);
            let original = tree_bytes(&data);
            let output =
                run_restore_in_child_copy_mode(&archive, &data, "cutover-validated", &[], false);
            assert_eq!(output.status.code(), Some(9));
            let mut journal = read_restore_journal(&data).unwrap();
            let prior = tmp.path().join("prior-retained-partial");
            std::fs::create_dir(&prior).unwrap();
            std::fs::write(prior.join("partial"), b"earlier partial bytes").unwrap();
            journal.previous_preserved_copies.push(prior.clone());
            publish_restore_phase(&journal).unwrap();
            let RestoreDisposition::PreserveLive { receipt } =
                journal.disposition.as_ref().unwrap()
            else {
                panic!()
            };
            let journal_before = std::fs::read(restore_journal_path(&data)).unwrap();
            let destination = tree_bytes(&data);
            let output = run_restore_in_child_copy_mode(
                &archive,
                &data,
                "unused",
                &[("NW_RESTORE_TEST_LIVE_IO_FAIL", fault)],
                false,
            );
            assert_eq!(output.status.code(), Some(3));
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert!(stderr.contains("typed_backup_refusal=false"), "{stderr}");
            for path in [&receipt.reservation, &receipt.preserved_path, &prior] {
                assert!(stderr.contains(&path.display().to_string()), "{stderr}");
            }
            assert_eq!(tree_bytes(&data), destination);
            assert_eq!(
                std::fs::read(restore_journal_path(&data)).unwrap(),
                journal_before
            );
            assert_eq!(
                std::fs::read(prior.join("partial")).unwrap(),
                b"earlier partial bytes"
            );
            let retained = if fault == "rename" {
                restoring_dir_for(&data)
            } else {
                receipt.preserved_path.clone()
            };
            assert_eq!(tree_bytes(&retained), original);
        }
    }

    #[test]
    fn valid_custom_basename_restore_can_be_restored_again_but_preserves_unknown_nested_files() {
        let tmp = tempfile::tempdir().unwrap();
        let (data, snapshot, _) = seed_marker_replay_dataset(tmp.path(), "graph.db");
        for _ in 0..2 {
            backup_restore(&RestoreConfig {
                snapshot_path: snapshot.clone(),
                data_dir: data.clone(),
                replace: true,
            })
            .unwrap();
            assert!(
                nestweaver_store::engine_format::has_current_file_marker(&data.join("graph.db"))
                    .unwrap()
            );
            assert!(is_recognisable_data_dir(&data).unwrap());
        }
        let original_hash = sha256_stream_path(data.join("graph.db")).unwrap();
        let personal = data.join("graph.db.publications").join("thesis.txt");
        std::fs::create_dir_all(personal.parent().unwrap()).unwrap();
        std::fs::write(&personal, b"irreplaceable").unwrap();
        let error = backup_restore(&RestoreConfig {
            snapshot_path: snapshot,
            data_dir: data.clone(),
            replace: true,
        })
        .unwrap_err();
        assert!(error.downcast_ref::<BackupRefusal>().is_some(), "{error:#}");
        assert_eq!(std::fs::read(personal).unwrap(), b"irreplaceable");
        assert_eq!(
            sha256_stream_path(data.join("graph.db")).unwrap(),
            original_hash
        );
    }

    #[test]
    fn finalized_marker_replay_is_idempotent_with_or_without_an_aside_copy() {
        for with_aside in [false, true] {
            let tmp = tempfile::tempdir().unwrap();
            let (data, snapshot, _) = seed_marker_replay_dataset(tmp.path(), "brain.lbug");
            let original_hash = sha256_stream_path(data.join("brain.lbug")).unwrap();
            if with_aside {
                seed_dataset(&restoring_dir_for(&data), b"SUPERSEDED");
            }
            marker_replay_journal(&data, &snapshot, "brain.lbug", RestorePhase::Cleanup);
            let immutable_receipt = read_restore_journal(&data)
                .unwrap()
                .destination_file_witnesses;
            std::fs::remove_file(snapshot).unwrap();
            for _ in 0..2 {
                recover_interrupted_restore(&data).unwrap();
            }
            assert!(
                nestweaver_store::engine_format::has_current_file_marker(&data.join("brain.lbug"))
                    .unwrap()
            );
            assert_eq!(
                sha256_stream_path(data.join("brain.lbug")).unwrap(),
                original_hash
            );
            assert_eq!(
                Some(restore_tree_witnesses(&data).unwrap()),
                immutable_receipt
            );
            assert!(!restoring_dir_for(&data).exists());
            assert!(!restore_journal_path(&data).exists());
        }
    }

    #[test]
    fn validated_marker_replay_preserves_replaced_graph_legacy_receipt_and_unknown_contents() {
        for fault in [
            "replaced_graph_and_archive",
            "legacy_receipt",
            "unknown_nested",
            "unbound_marker",
        ] {
            for with_aside in [false, true] {
                let tmp = tempfile::tempdir().unwrap();
                let (data, snapshot, copied_marker) =
                    seed_marker_replay_dataset(tmp.path(), "brain.lbug");
                let db = data.join("brain.lbug");
                let marker = nestweaver_store::engine_format::sidecar_path(&db);
                if with_aside {
                    seed_dataset(&restoring_dir_for(&data), b"ONLY INCUMBENT");
                }
                marker_replay_journal(
                    &data,
                    &snapshot,
                    "brain.lbug",
                    RestorePhase::CutoverValidated,
                );
                match fault {
                    "replaced_graph_and_archive" => {
                        let foreign_dir = tmp.path().join("foreign");
                        std::fs::create_dir(&foreign_dir).unwrap();
                        let foreign_db = foreign_dir.join("brain.lbug");
                        let foreign = nestweaver_store::GraphStore::create(&foreign_db).unwrap();
                        foreign.ensure_data_instance_id("foreign").unwrap();
                        drop(foreign);
                        let foreign_archive = tmp.path().join("foreign.nwsnap.zst");
                        backup_save(&BackupConfig {
                            db_path: foreign_db.clone(),
                            output_path: foreign_archive.clone(),
                            include_clones: false,
                            instance_id: "foreign".into(),
                            workspace_path: None,
                            overwrite: false,
                        })
                        .unwrap();
                        std::fs::copy(foreign_db, &db).unwrap();
                        std::fs::copy(foreign_archive, &snapshot).unwrap();
                    }
                    "legacy_receipt" => {
                        let mut journal = read_restore_journal(&data).unwrap();
                        journal.version = 2;
                        journal.disposition = None;
                        journal.source_checksums = None;
                        journal.graph_path = None;
                        journal.destination_file_witnesses = None;
                        publish_restore_phase(&journal).unwrap();
                    }
                    "unbound_marker" => {
                        std::fs::write(&marker, &copied_marker).unwrap();
                        assert!(
                            !nestweaver_store::engine_format::has_current_file_marker(&db).unwrap()
                        );
                    }
                    "unknown_nested" => {
                        let personal = data.join("brain.lbug.publications/thesis.txt");
                        std::fs::create_dir_all(personal.parent().unwrap()).unwrap();
                        std::fs::write(personal, b"mine").unwrap();
                    }
                    _ => unreachable!(),
                }
                let before_graph = sha256_stream_path(&db).unwrap();
                let before_marker = std::fs::read(&marker).unwrap();
                let before_journal = std::fs::read(restore_journal_path(&data)).unwrap();
                for _ in 0..2 {
                    assert!(
                        recover_interrupted_restore(&data).is_err(),
                        "{fault}, aside={with_aside}"
                    );
                    assert_eq!(sha256_stream_path(&db).unwrap(), before_graph);
                    assert_eq!(std::fs::read(&marker).unwrap(), before_marker);
                    assert_eq!(
                        std::fs::read(restore_journal_path(&data)).unwrap(),
                        before_journal
                    );
                    if with_aside {
                        assert_eq!(
                            std::fs::read(restoring_dir_for(&data).join("brain.lbug")).unwrap(),
                            b"ONLY INCUMBENT"
                        );
                    }
                    if fault == "unknown_nested" {
                        assert_eq!(
                            std::fs::read(data.join("brain.lbug.publications/thesis.txt")).unwrap(),
                            b"mine"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn lost_journal_before_marker_finalization_preserves_both_complete_copies() {
        let tmp = tempfile::tempdir().unwrap();
        let (data, _, copied_marker) = seed_marker_replay_dataset(tmp.path(), "brain.lbug");
        let db = data.join("brain.lbug");
        let marker = nestweaver_store::engine_format::sidecar_path(&db);
        std::fs::write(&marker, &copied_marker).unwrap();
        assert!(restored_dataset_is_self_proving(&data));
        let aside = restoring_dir_for(&data);
        std::fs::create_dir(&aside).unwrap();
        let incumbent = nestweaver_store::GraphStore::create(&aside.join("brain.lbug")).unwrap();
        incumbent.ensure_data_instance_id("last-incumbent").unwrap();
        drop(incumbent);
        let graph_hash = sha256_stream_path(&db).unwrap();
        let recovery = recover_interrupted_restore(&data).unwrap();
        let preserved = recovery
            .preserved
            .expect("unbound marker is not completion proof");
        assert_eq!(sha256_stream_path(&db).unwrap(), graph_hash);
        assert_eq!(std::fs::read(marker).unwrap(), copied_marker);
        assert_eq!(
            nestweaver_store::GraphStore::open_read_only(&preserved.join("brain.lbug"))
                .unwrap()
                .data_instance_id()
                .unwrap()
                .as_deref(),
            Some("last-incumbent")
        );
        recover_interrupted_restore(&data).unwrap();
        assert!(preserved.exists());
    }

    #[test]
    fn checksum_self_proof_without_journal_never_authorizes_deleting_an_owned_aside() {
        let tmp = tempfile::tempdir().unwrap();
        let (data, _, _) = seed_marker_replay_dataset(tmp.path(), "brain.lbug");
        let manifest_path = data.join("manifest.json");
        let mut manifest: BackupManifest =
            serde_json::from_slice(&std::fs::read(&manifest_path).unwrap()).unwrap();
        for (path, checksum) in &mut manifest.checksums {
            *checksum = sha256_stream_path(data.join(path)).unwrap();
        }
        std::fs::write(manifest_path, serde_json::to_vec(&manifest).unwrap()).unwrap();
        assert!(restored_dataset_is_self_proving(&data));
        assert!(is_recognisable_data_dir(&data).unwrap());
        let aside = restoring_dir_for(&data);
        std::fs::create_dir(&aside).unwrap();
        let old = nestweaver_store::GraphStore::create(&aside.join("brain.lbug")).unwrap();
        old.ensure_data_instance_id("unproven-owned-aside").unwrap();
        drop(old);
        assert!(is_recognisable_data_dir(&aside).unwrap());
        let destination_hash = sha256_stream_path(data.join("brain.lbug")).unwrap();
        let original_hash = sha256_stream_path(aside.join("brain.lbug")).unwrap();
        let recovery = recover_interrupted_restore(&data).unwrap();
        let preserved = recovery
            .preserved
            .expect("even marked aside needs independent original-file proof");
        assert_eq!(
            sha256_stream_path(data.join("brain.lbug")).unwrap(),
            destination_hash
        );
        assert_eq!(
            sha256_stream_path(preserved.join("brain.lbug")).unwrap(),
            original_hash
        );
        assert_eq!(
            nestweaver_store::GraphStore::open_read_only(&preserved.join("brain.lbug"))
                .unwrap()
                .data_instance_id()
                .unwrap()
                .as_deref(),
            Some("unproven-owned-aside")
        );
        recover_interrupted_restore(&data).unwrap();
        assert!(preserved.exists());
    }

    /// THE DEFECT, reproduced.
    ///
    /// A cross-device cutover that died after its first file leaves a NONEMPTY
    /// and worthless `data_dir` while `<data>.restoring` holds the only
    /// complete copy of the user's database. `recover_interrupted_restore`
    /// read that nonemptiness as "the cutover finished" and best-effort
    /// deleted the copy — so one partial file destroyed the sole good
    /// pre-restore dataset, before the retry that needed it.
    ///
    /// Against the unfixed function this fails with
    /// "recovery destroyed the only complete copy of the user's data".
    #[test]
    fn recovery_never_deletes_the_last_complete_copy_after_a_partial_copy() {
        let tmp = tempfile::TempDir::new().unwrap();
        let data = tmp.path().join("data");
        let restoring = tmp.path().join("data.restoring");

        seed_dataset(&restoring, b"THE ONLY COMPLETE COPY");
        // The partial destination. Nonempty; nothing more is true of it.
        std::fs::create_dir_all(&data).unwrap();
        std::fs::write(data.join("brain.lbug"), b"PARTIAL").unwrap();

        recover_interrupted_restore(&data).expect("recovery must not error");

        assert!(
            !copies_of(tmp.path(), b"THE ONLY COMPLETE COPY").is_empty(),
            "recovery destroyed the only complete copy of the user's data, on the sole \
             evidence that a partial destination was nonempty"
        );
    }

    /// With the durable journal saying the destination was still being built,
    /// recovery can do better than merely preserving: it KNOWS the destination
    /// is not authoritative, discards it, and reinstates the preserved copy.
    /// Repeating recovery changes nothing.
    #[test]
    fn a_journalled_partial_copy_is_discarded_and_the_preserved_copy_reinstated() {
        let tmp = tempfile::TempDir::new().unwrap();
        let data = tmp.path().join("data");
        let restoring = tmp.path().join("data.restoring");

        seed_dataset(&restoring, b"COMPLETE");
        std::fs::create_dir_all(&data).unwrap();
        std::fs::write(data.join("brain.lbug"), b"PARTIAL").unwrap();
        journal_at(&data, RestorePhase::CopyInProgress);

        for pass in 0..3 {
            recover_interrupted_restore(&data).unwrap_or_else(|e| panic!("pass {pass}: {e}"));
            assert_eq!(
                std::fs::read(data.join("brain.lbug")).unwrap(),
                b"COMPLETE",
                "pass {pass}: the preserved copy must be reinstated"
            );
            assert!(
                !restoring.exists(),
                "pass {pass}: the aside copy was consumed"
            );
            assert!(
                !restore_journal_path(&data).exists(),
                "pass {pass}: a reconciled journal must be retired"
            );
        }
    }

    /// The completed-cutover counterweight. The journal is published only
    /// after the replacement is durable AND validated, so at
    /// `CutoverValidated` the aside copy is PROVABLY redundant and removing it
    /// is correct. Without this the fix would simply never clean up.
    #[test]
    fn a_journalled_validated_cutover_retires_the_aside_copy() {
        let tmp = tempfile::TempDir::new().unwrap();
        let restoring = tmp.path().join("data.restoring");
        let (data, snapshot, _) = seed_marker_replay_dataset(tmp.path(), "brain.lbug");
        let original_hash = sha256_stream_path(data.join("brain.lbug")).unwrap();
        seed_dataset(&restoring, b"SUPERSEDED");
        marker_replay_journal(
            &data,
            &snapshot,
            "brain.lbug",
            RestorePhase::CutoverValidated,
        );
        for pass in 0..3 {
            recover_interrupted_restore(&data).unwrap_or_else(|e| panic!("pass {pass}: {e}"));
            assert!(
                !restoring.exists(),
                "pass {pass}: proven-redundant copy removed"
            );
            assert_eq!(
                sha256_stream_path(data.join("brain.lbug")).unwrap(),
                original_hash
            );
            assert!(!restore_journal_path(&data).exists(), "pass {pass}");
        }
    }

    /// A destination checksum manifest without an independently captured
    /// journal does not authorize deleting an unowned aside copy.
    #[test]
    fn a_self_proving_destination_preserves_an_unowned_aside_copy_without_a_journal() {
        let tmp = tempfile::TempDir::new().unwrap();
        let data = tmp.path().join("data");
        let restoring = tmp.path().join("data.restoring");

        seed_dataset(&data, b"RESTORED");
        seed_dataset(&restoring, b"SUPERSEDED");
        write_self_proof(&data);

        let recovery = recover_interrupted_restore(&data).unwrap();

        assert!(
            !restoring.exists(),
            "the orphan is moved to a reported preserved path"
        );
        let preserved = recovery
            .preserved
            .expect("replacement proof is not ownership of the old directory");
        assert_eq!(
            std::fs::read(preserved.join("brain.lbug")).unwrap(),
            b"SUPERSEDED"
        );
        assert_eq!(std::fs::read(data.join("brain.lbug")).unwrap(), b"RESTORED");
    }

    /// Nonempty, no journal, and no self-proof: nothing establishes that the
    /// destination is complete, so the aside copy is PRESERVED — renamed, not
    /// deleted — and the caller is told where it went.
    #[test]
    fn an_unprovable_destination_preserves_the_aside_copy_instead_of_deleting_it() {
        let tmp = tempfile::TempDir::new().unwrap();
        let data = tmp.path().join("data");
        let restoring = tmp.path().join("data.restoring");

        seed_dataset(&restoring, b"LAST GOOD COPY");
        std::fs::create_dir_all(&data).unwrap();
        std::fs::write(data.join("brain.lbug"), b"PARTIAL").unwrap();

        let recovery = recover_interrupted_restore(&data).unwrap();
        let preserved = recovery
            .preserved
            .expect("an unprovable copy must be reported, not silently kept");

        assert!(preserved.exists(), "the preserved copy must be on disk");
        assert_eq!(
            std::fs::read(preserved.join("brain.lbug")).unwrap(),
            b"LAST GOOD COPY"
        );
        assert!(
            !restoring.exists(),
            "`.restoring` must be freed so the next restore is not wedged"
        );

        // And a second restore is not wedged by it: `.restoring` is free.
        let second = recover_interrupted_restore(&data).unwrap();
        assert!(second.preserved.is_none(), "nothing left to preserve");
        assert_eq!(
            copies_of(tmp.path(), b"LAST GOOD COPY").len(),
            1,
            "repeated recovery must neither duplicate nor destroy the preserved copy"
        );
    }

    /// A journal this build cannot read must not be treated as a completion
    /// proof. Garbage routes to the no-proof branch, which preserves.
    #[test]
    fn an_unreadable_journal_is_not_a_completion_proof() {
        let tmp = tempfile::TempDir::new().unwrap();
        let data = tmp.path().join("data");
        let restoring = tmp.path().join("data.restoring");

        seed_dataset(&restoring, b"LAST GOOD COPY");
        std::fs::create_dir_all(&data).unwrap();
        std::fs::write(data.join("brain.lbug"), b"PARTIAL").unwrap();
        std::fs::write(restore_journal_path(&data), b"{not json").unwrap();

        let recovery = recover_interrupted_restore(&data).unwrap();
        assert!(recovery.preserved.is_some());
        assert_eq!(copies_of(tmp.path(), b"LAST GOOD COPY").len(), 1);
    }

    /// Give `dir` an embedded `manifest.json` that its own contents satisfy.
    fn write_self_proof(dir: &Path) {
        let mut checksums = HashMap::new();
        for name in ["brain.lbug", "brain.lbug.filemeta.json"] {
            checksums.insert(
                name.to_string(),
                sha256_stream_path(dir.join(name)).unwrap(),
            );
        }
        let manifest = BackupManifest {
            warnings: Vec::new(),
            version: 2,
            tier: "standard".to_string(),
            nestweaver_version: "test".to_string(),
            schema_version: 1,
            created_at: "1970-01-01T00:00:00Z".to_string(),
            instance_id: "test".to_string(),
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
            checksums,
        };
        std::fs::write(
            dir.join("manifest.json"),
            serde_json::to_vec(&manifest).unwrap(),
        )
        .unwrap();
    }

    // -----------------------------------------------------------------------
    // Real process death, at every cutover phase
    // -----------------------------------------------------------------------

    /// Re-invoked as a CHILD PROCESS by
    /// [`a_restore_killed_at_any_cutover_phase_leaves_a_complete_dataset`]; a
    /// no-op in a normal run. The child performs a real restore and dies with
    /// `_exit` at the phase named by `NW_RESTORE_TEST_CRASH_AT` — no
    /// unwinding, no destructors, no flushing. That is the point: a phase
    /// marker that is not fsynced does not survive the death it describes, and
    /// only a real death proves it does.
    #[test]
    fn restore_crash_child() {
        let (Ok(snapshot), Ok(data_dir)) = (
            std::env::var("NW_RESTORE_TEST_SNAPSHOT"),
            std::env::var("NW_RESTORE_TEST_DATA_DIR"),
        ) else {
            return;
        };
        if let Err(error) = backup_restore(&RestoreConfig {
            snapshot_path: PathBuf::from(snapshot),
            data_dir: PathBuf::from(data_dir),
            replace: true,
        }) {
            eprintln!("child restore returned an error: {error:#}");
            eprintln!(
                "typed_backup_refusal={}",
                error.downcast_ref::<BackupRefusal>().is_some()
            );
            // A distinct code, so the parent can tell "the restore refused"
            // from "the injected crash fired" (9) and from "it succeeded" (0).
            std::process::exit(3);
        }
    }

    fn run_restore_in_child(
        snapshot: &Path,
        data_dir: &Path,
        crash_at: &str,
    ) -> std::process::ExitStatus {
        run_restore_in_child_with(snapshot, data_dir, crash_at, &[])
    }

    fn run_restore_in_child_with(
        snapshot: &Path,
        data_dir: &Path,
        crash_at: &str,
        extra_env: &[(&str, &str)],
    ) -> std::process::ExitStatus {
        run_restore_in_child_with_output(snapshot, data_dir, crash_at, extra_env).status
    }

    fn run_restore_in_child_with_output(
        snapshot: &Path,
        data_dir: &Path,
        crash_at: &str,
        extra_env: &[(&str, &str)],
    ) -> std::process::Output {
        run_restore_in_child_copy_mode(snapshot, data_dir, crash_at, extra_env, true)
    }

    fn run_restore_in_child_copy_mode(
        snapshot: &Path,
        data_dir: &Path,
        crash_at: &str,
        extra_env: &[(&str, &str)],
        force_copy: bool,
    ) -> std::process::Output {
        let mut command =
            std::process::Command::new(std::env::current_exe().expect("this test binary"));
        command
            .args([
                "--exact",
                "--nocapture",
                "--test-threads=1",
                "backup::tests::restore_crash_child",
            ])
            .env("NW_RESTORE_TEST_SNAPSHOT", snapshot)
            .env("NW_RESTORE_TEST_DATA_DIR", data_dir)
            .env("NW_RESTORE_TEST_CRASH_AT", crash_at)
            // Force the copy fallback. A same-filesystem rename is atomic and
            // has no partial state to test; the copy is the unsafe half, and
            // the CLI's own path construction makes it very hard to reach on
            // purpose, so it is injected here rather than staged on a second
            // real filesystem.
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        if force_copy {
            command.env("NW_RESTORE_TEST_FORCE_COPY", "1");
        } else {
            command.env_remove("NW_RESTORE_TEST_FORCE_COPY");
        }
        for (key, value) in extra_env {
            command.env(key, value);
        }
        command.output().expect("spawn the crash child")
    }

    /// Kill a REAL restore process at every phase of a cross-device cutover
    /// and require that a complete, openable dataset is recoverable each time.
    ///
    /// Up to and including `copy-midway` the user's own pre-restore data must
    /// come back — nothing else can rebuild it. From `cutover-validated` on it
    /// may legitimately be gone, because the replacement has been proven
    /// complete and the archive can rebuild anything discarded.
    #[test]
    fn a_restore_killed_at_any_cutover_phase_leaves_a_complete_dataset() {
        for (crash_at, incumbent_must_survive) in [
            ("staging-verified", true),
            ("old-data-preserved", true),
            ("copy-in-progress", true),
            ("copy-midway", true),
            ("before-marker-rebind", true),
            ("after-marker-rebind", true),
            ("cutover-validated", false),
            ("cleanup", false),
        ] {
            let tmp = tempfile::tempdir().unwrap();

            // The archive to restore.
            let source_db = tmp.path().join("source").join("test.lbug");
            std::fs::create_dir_all(source_db.parent().unwrap()).unwrap();
            drop(nestweaver_store::GraphStore::create(&source_db).unwrap());
            let snapshot = tmp.path().join("test.nwsnap.zst");
            backup_save(&BackupConfig {
                db_path: source_db,
                output_path: snapshot.clone(),
                include_clones: false,
                instance_id: "crash-test".to_string(),
                workspace_path: None,
                overwrite: false,
            })
            .unwrap();

            // The incumbent dataset the restore is about to replace: a real
            // store, plus a marker that only the incumbent carries.
            let data_dir = tmp.path().join("data");
            std::fs::create_dir_all(&data_dir).unwrap();
            let incumbent =
                nestweaver_store::GraphStore::create(&data_dir.join("test.lbug")).unwrap();
            incumbent
                .ensure_data_instance_id("incumbent-restore-fixture")
                .unwrap();
            drop(incumbent);

            let status = run_restore_in_child(&snapshot, &data_dir, crash_at);
            assert_eq!(
                status.code(),
                Some(9),
                "the child must have died at {crash_at}, not exited normally"
            );

            recover_interrupted_restore(&data_dir)
                .unwrap_or_else(|error| panic!("recovery after {crash_at}: {error}"));

            assert!(
                nestweaver_store::GraphStore::open_read_only(&data_dir.join("test.lbug")).is_ok(),
                "after a death at {crash_at} the data directory must hold an openable graph"
            );
            if incumbent_must_survive {
                assert_eq!(
                    nestweaver_store::GraphStore::open_read_only(&data_dir.join("test.lbug"))
                        .unwrap()
                        .data_instance_id()
                        .unwrap()
                        .as_deref(),
                    Some("incumbent-restore-fixture"),
                    "a death at {crash_at} lost the user's own pre-restore data — the one \
                     thing no archive can rebuild"
                );
            }

            // And the retry works: recovery leaves a state a restore can run
            // from, with no wedged `.restoring` and no second death needed.
            backup_restore(&RestoreConfig {
                snapshot_path: snapshot.clone(),
                data_dir: data_dir.clone(),
                replace: true,
            })
            .unwrap_or_else(|error| panic!("retry after {crash_at}: {error}"));
            assert!(
                nestweaver_store::GraphStore::open_read_only(&data_dir.join("test.lbug")).is_ok(),
                "the retry after {crash_at} must leave an openable graph"
            );
            assert!(
                !restoring_dir_for(&data_dir).exists(),
                "a completed restore after {crash_at} must retire its aside copy"
            );
            assert!(!restore_journal_path(&data_dir).exists());
        }
    }

    #[test]
    fn rename_marker_finalization_interruptions_reinstate_the_incumbent() {
        for crash_at in ["before-marker-rebind", "after-marker-rebind"] {
            let tmp = tempfile::tempdir().unwrap();
            let (data, snapshot, _) = seed_marker_replay_dataset(tmp.path(), "brain.lbug");
            std::fs::remove_dir_all(&data).unwrap();
            std::fs::create_dir(&data).unwrap();
            let incumbent = nestweaver_store::GraphStore::create(&data.join("brain.lbug")).unwrap();
            incumbent
                .ensure_data_instance_id("rename-incumbent")
                .unwrap();
            drop(incumbent);
            let output = run_restore_in_child_copy_mode(&snapshot, &data, crash_at, &[], false);
            assert_eq!(
                output.status.code(),
                Some(9),
                "{crash_at}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert_eq!(
                read_restore_journal(&data).unwrap().phase,
                RestorePhase::OldDataPreserved
            );
            for _ in 0..2 {
                recover_interrupted_restore(&data).unwrap();
            }
            assert_eq!(
                nestweaver_store::GraphStore::open_read_only(&data.join("brain.lbug"))
                    .unwrap()
                    .data_instance_id()
                    .unwrap()
                    .as_deref(),
                Some("rename-incumbent")
            );
            assert!(!restore_journal_path(&data).exists());
            backup_restore(&RestoreConfig {
                snapshot_path: snapshot,
                data_dir: data.clone(),
                replace: true,
            })
            .unwrap();
            assert!(
                nestweaver_store::engine_format::has_current_file_marker(&data.join("brain.lbug"))
                    .unwrap()
            );
        }
    }

    #[test]
    fn rollback_preserves_complete_destination_when_aside_is_exchanged_or_incomplete_without_archive_access()
     {
        for force_copy in [false, true] {
            for exchanged in [false, true] {
                let tmp = tempfile::tempdir().unwrap();
                let (data, snapshot, _) = seed_marker_replay_dataset(tmp.path(), "brain.lbug");
                let output = run_restore_in_child_copy_mode(
                    &snapshot,
                    &data,
                    "after-marker-rebind",
                    &[],
                    force_copy,
                );
                assert_eq!(
                    output.status.code(),
                    Some(9),
                    "{}",
                    String::from_utf8_lossy(&output.stderr)
                );
                let expected_phase = if force_copy {
                    RestorePhase::CopyInProgress
                } else {
                    RestorePhase::OldDataPreserved
                };
                assert_eq!(read_restore_journal(&data).unwrap().phase, expected_phase);
                let db = data.join("brain.lbug");
                assert!(nestweaver_store::engine_format::has_current_file_marker(&db).unwrap());
                assert!(nestweaver_store::GraphStore::open_read_only(&db).is_ok());
                let destination_hash = sha256_stream_path(&db).unwrap();
                let marker = nestweaver_store::engine_format::sidecar_path(&db);
                let destination_marker = std::fs::read(&marker).unwrap();
                let journal_bytes = std::fs::read(restore_journal_path(&data)).unwrap();
                let journal = read_restore_journal(&data).unwrap();
                let aside = restoring_dir_for(&data);
                let original_aside = tmp.path().join("original-aside");
                let original_hash = sha256_stream_path(aside.join("brain.lbug")).unwrap();
                if exchanged {
                    std::fs::rename(&aside, &original_aside).unwrap();
                    seed_dataset(&aside, b"FOREIGN ASIDE");
                } else {
                    std::fs::remove_file(aside.join("brain.lbug")).unwrap();
                    assert_eq!(
                        restore_directory_identity(&aside),
                        journal.restoring_identity
                    );
                    assert!(
                        journal_proves_owned_tree(
                            &aside,
                            journal.restoring_identity,
                            &journal.restoring_paths
                        ),
                        "a subset proof would incorrectly authorize rollback"
                    );
                }
                let remaining_aside_marker = if exchanged {
                    None
                } else {
                    Some(
                        std::fs::read(nestweaver_store::engine_format::sidecar_path(
                            &aside.join("brain.lbug"),
                        ))
                        .unwrap(),
                    )
                };
                std::fs::remove_file(&snapshot).unwrap();
                for _ in 0..2 {
                    let error = recover_interrupted_restore(&data).unwrap_err();
                    assert!(error.downcast_ref::<BackupRefusal>().is_some(), "{error:#}");
                    assert!(
                        format!("{error:#}").contains("preserved aside copy"),
                        "{error:#}"
                    );
                    assert_eq!(sha256_stream_path(&db).unwrap(), destination_hash);
                    assert_eq!(std::fs::read(&marker).unwrap(), destination_marker);
                    assert_eq!(
                        std::fs::read(restore_journal_path(&data)).unwrap(),
                        journal_bytes
                    );
                    if exchanged {
                        assert_eq!(
                            std::fs::read(aside.join("brain.lbug")).unwrap(),
                            b"FOREIGN ASIDE"
                        );
                        assert_eq!(
                            sha256_stream_path(original_aside.join("brain.lbug")).unwrap(),
                            original_hash
                        );
                    } else {
                        assert!(aside.exists());
                        assert!(!aside.join("brain.lbug").exists());
                        assert_eq!(
                            std::fs::read(nestweaver_store::engine_format::sidecar_path(
                                &aside.join("brain.lbug")
                            ))
                            .unwrap(),
                            *remaining_aside_marker.as_ref().unwrap()
                        );
                    }
                    assert!(nestweaver_store::GraphStore::open_read_only(&db).is_ok());
                }
            }
        }
    }

    #[test]
    fn original_aside_file_witnesses_refuse_same_name_replacements_and_missing_proof() {
        for phase in [
            RestorePhase::OldDataPreserved,
            RestorePhase::CopyInProgress,
            RestorePhase::CutoverValidated,
            RestorePhase::Cleanup,
        ] {
            for replace_content in [false, true] {
                let tmp = tempfile::tempdir().unwrap();
                let (data, snapshot, _) = seed_marker_replay_dataset(tmp.path(), "brain.lbug");
                let (crash_at, force_copy) = match phase {
                    RestorePhase::OldDataPreserved => ("after-marker-rebind", false),
                    RestorePhase::CopyInProgress => ("after-marker-rebind", true),
                    RestorePhase::CutoverValidated => ("cutover-validated", true),
                    RestorePhase::Cleanup => ("cleanup", true),
                    _ => unreachable!(),
                };
                let output =
                    run_restore_in_child_copy_mode(&snapshot, &data, crash_at, &[], force_copy);
                assert_eq!(
                    output.status.code(),
                    Some(9),
                    "{phase:?}: {}",
                    String::from_utf8_lossy(&output.stderr)
                );
                let mut journal = read_restore_journal(&data).unwrap();
                assert_eq!(journal.phase, phase);
                let aside = restoring_dir_for(&data);
                let aside_db = aside.join("brain.lbug");
                if replace_content {
                    let foreign_dir = tmp.path().join("foreign");
                    std::fs::create_dir(&foreign_dir).unwrap();
                    let foreign_db = foreign_dir.join("brain.lbug");
                    let foreign = nestweaver_store::GraphStore::create(&foreign_db).unwrap();
                    foreign.ensure_data_instance_id("foreign-aside").unwrap();
                    drop(foreign);
                    // In-place replacement keeps the original graph inode;
                    // a freshly valid marker still cannot establish origin.
                    std::fs::copy(&foreign_db, &aside_db).unwrap();
                    nestweaver_store::engine_format::write_sidecar(&aside_db).unwrap();
                    assert!(
                        nestweaver_store::engine_format::has_current_file_marker(&aside_db)
                            .unwrap()
                    );
                    assert_eq!(
                        restore_directory_identity(&aside),
                        journal.restoring_identity
                    );
                    assert!(journal_proves_complete_owned_tree(
                        &aside,
                        journal.restoring_identity,
                        &journal.restoring_paths
                    ));
                    assert_ne!(
                        restore_file_witness(&aside_db).unwrap(),
                        *journal
                            .restoring_file_witnesses
                            .as_ref()
                            .unwrap()
                            .get("brain.lbug")
                            .unwrap()
                    );
                } else {
                    journal.restoring_file_witnesses = None;
                    publish_restore_phase(&journal).unwrap();
                }
                let db = data.join("brain.lbug");
                let destination_hash = sha256_stream_path(&db).unwrap();
                let destination_marker_path = nestweaver_store::engine_format::sidecar_path(&db);
                let destination_marker = std::fs::read(&destination_marker_path).unwrap();
                let aside_hash = sha256_stream_path(&aside_db).unwrap();
                let aside_marker_path = nestweaver_store::engine_format::sidecar_path(&aside_db);
                let aside_marker = std::fs::read(&aside_marker_path).unwrap();
                let journal_bytes = std::fs::read(restore_journal_path(&data)).unwrap();
                std::fs::remove_file(snapshot).unwrap();
                for _ in 0..2 {
                    let error = recover_interrupted_restore(&data).unwrap_err();
                    assert!(
                        error.downcast_ref::<BackupRefusal>().is_some(),
                        "{phase:?}: {error:#}"
                    );
                    assert_eq!(sha256_stream_path(&db).unwrap(), destination_hash);
                    assert_eq!(
                        std::fs::read(&destination_marker_path).unwrap(),
                        destination_marker
                    );
                    assert_eq!(sha256_stream_path(&aside_db).unwrap(), aside_hash);
                    assert_eq!(std::fs::read(&aside_marker_path).unwrap(), aside_marker);
                    assert_eq!(
                        std::fs::read(restore_journal_path(&data)).unwrap(),
                        journal_bytes
                    );
                    assert!(nestweaver_store::GraphStore::open_read_only(&db).is_ok());
                    assert!(nestweaver_store::GraphStore::open_read_only(&aside_db).is_ok());
                }
            }
        }
    }

    #[test]
    fn validated_cleanup_accepts_only_original_surviving_aside_files() {
        let tmp = tempfile::tempdir().unwrap();
        let (data, snapshot, _) = seed_marker_replay_dataset(tmp.path(), "brain.lbug");
        let output = run_restore_in_child_copy_mode(&snapshot, &data, "cleanup", &[], true);
        assert_eq!(
            output.status.code(),
            Some(9),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let journal = read_restore_journal(&data).unwrap();
        let aside = restoring_dir_for(&data);
        std::fs::remove_file(aside.join("brain.lbug")).unwrap();
        assert!(!journal_proves_original_aside(&aside, &journal, true));
        assert!(journal_proves_original_aside(&aside, &journal, false));
        let destination_hash = sha256_stream_path(data.join("brain.lbug")).unwrap();
        for _ in 0..2 {
            recover_interrupted_restore(&data).unwrap();
        }
        assert!(!aside.exists());
        assert!(!restore_journal_path(&data).exists());
        assert_eq!(
            sha256_stream_path(data.join("brain.lbug")).unwrap(),
            destination_hash
        );
        assert!(
            nestweaver_store::engine_format::has_current_file_marker(&data.join("brain.lbug"))
                .unwrap()
        );
    }

    #[test]
    fn restored_operational_lock_refuses_foreign_bytes_directories_and_links() {
        for fault in [
            "foreign_bytes",
            "unknown_empty",
            "directory",
            "symlink",
            "hardlink",
        ] {
            let tmp = tempfile::tempdir().unwrap();
            let (data, snapshot, _) = seed_marker_replay_dataset(tmp.path(), "brain.lbug");
            marker_replay_journal(
                &data,
                &snapshot,
                "brain.lbug",
                RestorePhase::OldDataPreserved,
            );
            let mut journal = read_restore_journal(&data).unwrap();
            let lock = data.join("brain.lbug.write.lock");
            std::fs::remove_file(&lock).unwrap();
            journal
                .destination_paths
                .retain(|path| path != "brain.lbug.write.lock");
            journal
                .destination_file_witnesses
                .as_mut()
                .unwrap()
                .remove("brain.lbug.write.lock");
            publish_restore_phase(&journal).unwrap();
            let allowed_before = journal.destination_paths.clone();
            let outside = tmp.path().join("outside");
            std::fs::write(&outside, b"").unwrap();
            match fault {
                "foreign_bytes" => std::fs::write(&lock, b"irreplaceable").unwrap(),
                "unknown_empty" => std::fs::write(&lock, b"").unwrap(),
                "directory" => std::fs::create_dir(&lock).unwrap(),
                "symlink" => {
                    #[cfg(unix)]
                    std::os::unix::fs::symlink(&outside, &lock).unwrap();
                    #[cfg(not(unix))]
                    continue;
                }
                "hardlink" => {
                    std::fs::hard_link(&outside, &lock).unwrap();
                }
                _ => unreachable!(),
            }
            let destination_hash = sha256_stream_path(data.join("brain.lbug")).unwrap();
            let journal_bytes = std::fs::read(restore_journal_path(&data)).unwrap();
            assert!(
                prepare_restored_write_lock(&data, &mut journal).is_err(),
                "{fault}"
            );
            assert_eq!(
                sha256_stream_path(data.join("brain.lbug")).unwrap(),
                destination_hash
            );
            assert_eq!(
                std::fs::read(restore_journal_path(&data)).unwrap(),
                journal_bytes
            );
            assert_eq!(journal.destination_paths, allowed_before);
            for _ in 0..2 {
                assert!(recover_interrupted_restore(&data).is_err(), "{fault}");
            }
            assert_eq!(
                std::fs::read(restore_journal_path(&data)).unwrap(),
                journal_bytes
            );
            assert_eq!(std::fs::read(outside).unwrap().as_slice(), b"");
            match fault {
                "foreign_bytes" => assert_eq!(std::fs::read(&lock).unwrap(), b"irreplaceable"),
                "unknown_empty" => assert_eq!(std::fs::read(&lock).unwrap().as_slice(), b""),
                "directory" => assert!(lock.is_dir()),
                "symlink" => assert!(
                    std::fs::symlink_metadata(&lock)
                        .unwrap()
                        .file_type()
                        .is_symlink()
                ),
                "hardlink" => assert_eq!(std::fs::read(&lock).unwrap().as_slice(), b""),
                _ => unreachable!(),
            }
        }
    }

    #[test]
    fn existing_operational_lock_requires_its_exact_previously_finalized_witness() {
        let tmp = tempfile::tempdir().unwrap();
        let (data, snapshot, _) = seed_marker_replay_dataset(tmp.path(), "brain.lbug");
        marker_replay_journal(
            &data,
            &snapshot,
            "brain.lbug",
            RestorePhase::CutoverValidated,
        );
        let mut journal = read_restore_journal(&data).unwrap();
        let lock = data.join("brain.lbug.write.lock");
        let original = restore_file_witness(&lock).unwrap();
        let journal_bytes = std::fs::read(restore_journal_path(&data)).unwrap();
        prepare_restored_write_lock(&data, &mut journal).unwrap();
        assert_eq!(restore_file_witness(&lock).unwrap(), original);
        assert_eq!(
            std::fs::read(restore_journal_path(&data)).unwrap(),
            journal_bytes
        );
        let replacement = data.join("replacement-lock");
        std::fs::write(&replacement, b"").unwrap();
        std::fs::rename(replacement, &lock).unwrap();
        assert_ne!(restore_file_witness(&lock).unwrap(), original);
        assert!(prepare_restored_write_lock(&data, &mut journal).is_err());
        assert_eq!(std::fs::read(&lock).unwrap().as_slice(), b"");
        assert_eq!(
            std::fs::read(restore_journal_path(&data)).unwrap(),
            journal_bytes
        );
        assert!(recover_interrupted_restore(&data).is_err());
        assert_eq!(
            std::fs::read(restore_journal_path(&data)).unwrap(),
            journal_bytes
        );
    }

    #[test]
    fn source_inventoried_empty_operational_lock_is_admitted_without_finalized_witness() {
        let tmp = tempfile::tempdir().unwrap();
        let (data, snapshot, _) = seed_marker_replay_dataset(tmp.path(), "brain.lbug");
        marker_replay_journal(&data, &snapshot, "brain.lbug", RestorePhase::CopyInProgress);
        let mut journal = read_restore_journal(&data).unwrap();
        journal.destination_file_witnesses = None;
        journal.source_checksums.as_mut().unwrap().insert(
            "brain.lbug.write.lock".into(),
            sha256_stream(std::io::empty()).unwrap(),
        );
        publish_restore_phase(&journal).unwrap();
        let before = std::fs::read(restore_journal_path(&data)).unwrap();
        let lock = data.join("brain.lbug.write.lock");
        let witness = restore_file_witness(&lock).unwrap();
        prepare_restored_write_lock(&data, &mut journal).unwrap();
        assert_eq!(restore_file_witness(&lock).unwrap(), witness);
        assert_eq!(std::fs::read(restore_journal_path(&data)).unwrap(), before);
    }

    #[test]
    fn competing_operational_lock_creator_never_receives_recovery_deletion_authority() {
        let tmp = tempfile::tempdir().unwrap();
        let (data, snapshot, _) = seed_marker_replay_dataset(tmp.path(), "brain.lbug");
        let output = run_restore_in_child_copy_mode(
            &snapshot,
            &data,
            "never",
            &[("NW_RESTORE_TEST_OPERATIONAL_LOCK_RACE", "1")],
            true,
        );
        assert_eq!(
            output.status.code(),
            Some(3),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            String::from_utf8_lossy(&output.stderr)
                .contains("foreign operational lock appeared during creation")
        );
        let journal = read_restore_journal(&data).unwrap();
        assert_eq!(journal.phase, RestorePhase::CopyInProgress);
        assert!(
            !journal
                .destination_paths
                .contains(&"brain.lbug.write.lock".to_string())
        );
        let lock = data.join("brain.lbug.write.lock");
        let foreign_bytes = std::fs::read(&lock).unwrap();
        assert_eq!(foreign_bytes, b"foreign creator's irreplaceable bytes");
        let graph_hash = sha256_stream_path(data.join("brain.lbug")).unwrap();
        let original_hash =
            sha256_stream_path(restoring_dir_for(&data).join("brain.lbug")).unwrap();
        let journal_bytes = std::fs::read(restore_journal_path(&data)).unwrap();
        for _ in 0..2 {
            assert!(
                backup_restore(&RestoreConfig {
                    snapshot_path: snapshot.clone(),
                    data_dir: data.clone(),
                    replace: true
                })
                .is_err()
            );
            assert_eq!(std::fs::read(&lock).unwrap(), foreign_bytes);
            assert_eq!(
                sha256_stream_path(data.join("brain.lbug")).unwrap(),
                graph_hash
            );
            assert_eq!(
                sha256_stream_path(restoring_dir_for(&data).join("brain.lbug")).unwrap(),
                original_hash
            );
            assert_eq!(
                std::fs::read(restore_journal_path(&data)).unwrap(),
                journal_bytes
            );
        }
    }

    #[test]
    fn public_restore_retries_validated_cutovers_without_private_recovery_first() {
        for force_copy in [false, true] {
            for with_aside in [false, true] {
                for crash_at in ["cutover-validated", "cleanup"] {
                    if !with_aside && crash_at == "cleanup" {
                        continue;
                    }
                    let tmp = tempfile::tempdir().unwrap();
                    let (data, snapshot, _) = seed_marker_replay_dataset(tmp.path(), "brain.lbug");
                    if !with_aside {
                        std::fs::remove_dir_all(&data).unwrap();
                    }
                    let output =
                        run_restore_in_child_copy_mode(&snapshot, &data, crash_at, &[], force_copy);
                    assert_eq!(
                        output.status.code(),
                        Some(9),
                        "{crash_at}, copy={force_copy}, aside={with_aside}: {}",
                        String::from_utf8_lossy(&output.stderr)
                    );
                    let receipt = read_restore_journal(&data).unwrap();
                    assert_eq!(restoring_dir_for(&data).exists(), with_aside);
                    let lock = data.join("brain.lbug.write.lock");
                    assert_eq!(std::fs::symlink_metadata(&lock).unwrap().len(), 0);
                    assert!(
                        receipt
                            .destination_paths
                            .contains(&"brain.lbug.write.lock".to_string())
                    );
                    assert!(
                        receipt
                            .destination_file_witnesses
                            .as_ref()
                            .unwrap()
                            .contains_key("brain.lbug.write.lock")
                    );
                    // Public entry acquires all leases before internal recovery.
                    // No helper may pre-clear the old journal or prepare its lock.
                    backup_restore(&RestoreConfig {
                        snapshot_path: snapshot.clone(),
                        data_dir: data.clone(),
                        replace: true,
                    })
                    .unwrap();
                    assert!(!restore_journal_path(&data).exists());
                    assert!(!restoring_dir_for(&data).exists());
                    assert!(
                        nestweaver_store::engine_format::has_current_file_marker(
                            &data.join("brain.lbug")
                        )
                        .unwrap()
                    );
                    assert!(
                        nestweaver_store::GraphStore::open_read_only(&data.join("brain.lbug"))
                            .is_ok()
                    );
                }
            }
        }
    }

    #[test]
    fn unvalidated_recovery_preserves_same_directory_replacement_graph_and_fresh_marker() {
        for force_copy in [false, true] {
            for with_aside in [false, true] {
                let tmp = tempfile::tempdir().unwrap();
                let (data, snapshot, _) = seed_marker_replay_dataset(tmp.path(), "brain.lbug");
                std::fs::remove_dir_all(&data).unwrap();
                if with_aside {
                    std::fs::create_dir(&data).unwrap();
                    let incumbent =
                        nestweaver_store::GraphStore::create(&data.join("brain.lbug")).unwrap();
                    incumbent
                        .ensure_data_instance_id("complete-incumbent")
                        .unwrap();
                    drop(incumbent);
                }
                let output = run_restore_in_child_copy_mode(
                    &snapshot,
                    &data,
                    "after-marker-rebind",
                    &[],
                    force_copy,
                );
                assert_eq!(
                    output.status.code(),
                    Some(9),
                    "{}",
                    String::from_utf8_lossy(&output.stderr)
                );
                let journal = read_restore_journal(&data).unwrap();
                let foreign_dir = tmp.path().join("foreign");
                std::fs::create_dir(&foreign_dir).unwrap();
                let foreign_db = foreign_dir.join("brain.lbug");
                let foreign = nestweaver_store::GraphStore::create(&foreign_db).unwrap();
                foreign
                    .ensure_data_instance_id("new-same-name-data")
                    .unwrap();
                drop(foreign);
                let db = data.join("brain.lbug");
                std::fs::copy(&foreign_db, &db).unwrap();
                nestweaver_store::engine_format::write_sidecar(&db).unwrap();
                assert_eq!(
                    restore_directory_identity(&data),
                    journal.destination_identity
                );
                assert!(journal_proves_owned_tree(
                    &data,
                    journal.destination_identity,
                    &journal.destination_paths
                ));
                assert!(nestweaver_store::engine_format::has_current_file_marker(&db).unwrap());
                let replacement_hash = sha256_stream_path(&db).unwrap();
                let replacement_marker =
                    std::fs::read(nestweaver_store::engine_format::sidecar_path(&db)).unwrap();
                std::fs::remove_file(snapshot).unwrap();
                let recovery = recover_interrupted_restore(&data).unwrap();
                let preserved = recovery
                    .preserved
                    .expect("unvalidated destination must be retained, not deleted");
                assert_eq!(
                    sha256_stream_path(preserved.join("brain.lbug")).unwrap(),
                    replacement_hash
                );
                assert_eq!(
                    std::fs::read(nestweaver_store::engine_format::sidecar_path(
                        &preserved.join("brain.lbug")
                    ))
                    .unwrap(),
                    replacement_marker
                );
                assert_eq!(
                    nestweaver_store::GraphStore::open_read_only(&preserved.join("brain.lbug"))
                        .unwrap()
                        .data_instance_id()
                        .unwrap()
                        .as_deref(),
                    Some("new-same-name-data")
                );
                if with_aside {
                    assert_eq!(
                        nestweaver_store::GraphStore::open_read_only(&db)
                            .unwrap()
                            .data_instance_id()
                            .unwrap()
                            .as_deref(),
                        Some("complete-incumbent")
                    );
                } else {
                    assert!(!data.exists());
                }
                recover_interrupted_restore(&data).unwrap();
                assert!(preserved.exists());
                assert_eq!(
                    sha256_stream_path(preserved.join("brain.lbug")).unwrap(),
                    replacement_hash
                );
                assert!(!restore_journal_path(&data).exists());
            }
        }
    }

    #[test]
    fn public_early_and_midcopy_recovery_retains_every_partial_byte_and_complete_incumbent() {
        for crash_at in ["copy-in-progress", "copy-midway"] {
            let tmp = tempfile::tempdir().unwrap();
            let (data, snapshot, _) = seed_marker_replay_dataset(tmp.path(), "brain.lbug");
            std::fs::remove_dir_all(&data).unwrap();
            std::fs::create_dir(&data).unwrap();
            let incumbent = nestweaver_store::GraphStore::create(&data.join("brain.lbug")).unwrap();
            incumbent
                .ensure_data_instance_id("early-copy-incumbent")
                .unwrap();
            drop(incumbent);
            let output = run_restore_in_child_copy_mode(&snapshot, &data, crash_at, &[], true);
            assert_eq!(
                output.status.code(),
                Some(9),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            let partial: Vec<_> = walkdir::WalkDir::new(&data)
                .into_iter()
                .map(Result::unwrap)
                .filter(|entry| entry.file_type().is_file())
                .map(|entry| {
                    (
                        entry.path().strip_prefix(&data).unwrap().to_path_buf(),
                        std::fs::read(entry.path()).unwrap(),
                    )
                })
                .collect();
            if crash_at == "copy-midway" {
                assert!(
                    !partial.is_empty(),
                    "midcopy seam must land real payload bytes"
                );
            }
            let aside = restoring_dir_for(&data);
            let incumbent_hash = sha256_stream_path(aside.join("brain.lbug")).unwrap();
            let error = backup_restore(&RestoreConfig {
                snapshot_path: tmp.path().join("deliberately-missing.nwsnap.zst"),
                data_dir: data.clone(),
                replace: true,
            })
            .unwrap_err();
            let incumbent_home = if aside.exists() {
                assert!(
                    format!("{error:#}").contains("stale or ambiguous recovery journal"),
                    "{error:#}"
                );
                aside
            } else {
                assert!(format!("{error:#}").contains("No such file"), "{error:#}");
                assert!(
                    format!("{error:#}").contains("unproven restore copy preserved at"),
                    "{error:#}"
                );
                data.clone()
            };
            assert_eq!(
                sha256_stream_path(incumbent_home.join("brain.lbug")).unwrap(),
                incumbent_hash
            );
            assert_eq!(
                nestweaver_store::GraphStore::open_read_only(&incumbent_home.join("brain.lbug"))
                    .unwrap()
                    .data_instance_id()
                    .unwrap()
                    .as_deref(),
                Some("early-copy-incumbent")
            );
            let candidates: Vec<_> = std::fs::read_dir(tmp.path())
                .unwrap()
                .map(Result::unwrap)
                .map(|entry| entry.path())
                .filter(|path| {
                    path == &data
                        || path
                            .file_name()
                            .unwrap()
                            .to_string_lossy()
                            .starts_with("data.preserved-")
                })
                .collect();
            assert!(
                !candidates.is_empty(),
                "partial destination must remain at a discoverable path"
            );
            assert!(
                candidates.iter().any(|candidate| partial
                    .iter()
                    .all(|(relative, bytes)| std::fs::read(candidate.join(relative))
                        .is_ok_and(|actual| actual == *bytes))),
                "all partial bytes must survive together"
            );
        }
    }

    #[test]
    fn public_force_restore_preserves_foreign_aside_after_success_and_later_archive_failure() {
        for proof in ["none", "v2_missing_witness", "stale_file_witness"] {
            for empty_target in [false, true] {
                for missing_archive in [false, true] {
                    let tmp = tempfile::tempdir().unwrap();
                    let (data, snapshot, _) = seed_marker_replay_dataset(tmp.path(), "brain.lbug");
                    std::fs::remove_dir_all(&data).unwrap();
                    if empty_target {
                        std::fs::create_dir(&data).unwrap();
                    }
                    let aside = restoring_dir_for(&data);
                    std::fs::create_dir(&aside).unwrap();
                    let aside_db = aside.join("brain.lbug");
                    let foreign = nestweaver_store::GraphStore::create(&aside_db).unwrap();
                    foreign.ensure_data_instance_id("foreign-aside").unwrap();
                    drop(foreign);
                    if proof != "none" {
                        journal_at(&data, RestorePhase::StagingVerified);
                        let mut legacy = read_restore_journal(&data).unwrap();
                        legacy.version = 3;
                        legacy.disposition = None;
                        publish_restore_phase(&legacy).unwrap();
                        if proof == "v2_missing_witness" {
                            let mut journal = read_restore_journal(&data).unwrap();
                            journal.version = 2;
                            journal.disposition = None;
                            journal.restoring_file_witnesses = None;
                            publish_restore_phase(&journal).unwrap();
                        } else {
                            let replacement_dir = tmp.path().join("replacement");
                            std::fs::create_dir(&replacement_dir).unwrap();
                            let replacement_db = replacement_dir.join("brain.lbug");
                            let replacement =
                                nestweaver_store::GraphStore::create(&replacement_db).unwrap();
                            replacement
                                .ensure_data_instance_id("foreign-aside")
                                .unwrap();
                            drop(replacement);
                            std::fs::copy(replacement_db, &aside_db).unwrap();
                            nestweaver_store::engine_format::write_sidecar(&aside_db).unwrap();
                        }
                    }
                    assert!(
                        nestweaver_store::engine_format::has_current_file_marker(&aside_db)
                            .unwrap()
                    );
                    let original_hash = sha256_stream_path(&aside_db).unwrap();
                    let original_marker =
                        std::fs::read(nestweaver_store::engine_format::sidecar_path(&aside_db))
                            .unwrap();
                    let requested = if missing_archive {
                        tmp.path().join("missing.nwsnap.zst")
                    } else {
                        snapshot
                    };
                    let outcome = backup_restore(&RestoreConfig {
                        snapshot_path: requested,
                        data_dir: data.clone(),
                        replace: true,
                    });
                    let preserved = if missing_archive {
                        let error = outcome.unwrap_err();
                        let candidates: Vec<_> = std::fs::read_dir(tmp.path())
                            .unwrap()
                            .map(Result::unwrap)
                            .map(|entry| entry.path())
                            .filter(|path| {
                                path.file_name()
                                    .unwrap()
                                    .to_string_lossy()
                                    .starts_with("data.restoring.preserved-")
                            })
                            .collect();
                        assert_eq!(candidates.len(), 1);
                        assert!(
                            format!("{error:#}").contains(&format!(
                                "unproven restore copy preserved at {}",
                                candidates[0].display()
                            )),
                            "{error:#}"
                        );
                        candidates[0].clone()
                    } else {
                        let result = outcome.unwrap();
                        assert_eq!(
                            nestweaver_store::GraphStore::open_read_only(&data.join("brain.lbug"))
                                .unwrap()
                                .data_instance_id()
                                .unwrap()
                                .as_deref(),
                            Some("default")
                        );
                        result
                            .preserved_copy
                            .expect("whole public restore must disclose retained foreign copy")
                    };
                    assert_ne!(preserved, data);
                    assert_ne!(preserved, aside);
                    assert!(!aside.exists());
                    assert_eq!(
                        sha256_stream_path(preserved.join("brain.lbug")).unwrap(),
                        original_hash
                    );
                    assert_eq!(
                        std::fs::read(nestweaver_store::engine_format::sidecar_path(
                            &preserved.join("brain.lbug")
                        ))
                        .unwrap(),
                        original_marker
                    );
                    assert_eq!(
                        nestweaver_store::GraphStore::open_read_only(&preserved.join("brain.lbug"))
                            .unwrap()
                            .data_instance_id()
                            .unwrap()
                            .as_deref(),
                        Some("foreign-aside")
                    );
                    recover_interrupted_restore(&data).unwrap();
                    assert_eq!(
                        sha256_stream_path(preserved.join("brain.lbug")).unwrap(),
                        original_hash
                    );
                }
            }
        }
    }

    #[test]
    fn captured_staging_verified_original_aside_is_promoted_after_real_rename_interruption() {
        let tmp = tempfile::tempdir().unwrap();
        let (data, snapshot, _) = seed_marker_replay_dataset(tmp.path(), "brain.lbug");
        std::fs::remove_dir_all(&data).unwrap();
        std::fs::create_dir(&data).unwrap();
        let incumbent = nestweaver_store::GraphStore::create(&data.join("brain.lbug")).unwrap();
        incumbent
            .ensure_data_instance_id("captured-original")
            .unwrap();
        drop(incumbent);
        let output = run_restore_in_child_copy_mode(
            &snapshot,
            &data,
            "old-data-renamed-before-journal",
            &[],
            true,
        );
        assert_eq!(
            output.status.code(),
            Some(9),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(!data.exists());
        let journal = read_restore_journal(&data).unwrap();
        assert_eq!(journal.phase, RestorePhase::StagingVerified);
        assert!(journal_proves_original_aside(
            &restoring_dir_for(&data),
            &journal,
            true
        ));
        let error = backup_restore(&RestoreConfig {
            snapshot_path: tmp.path().join("missing.nwsnap.zst"),
            data_dir: data.clone(),
            replace: true,
        })
        .unwrap_err();
        assert!(format!("{error:#}").contains("No such file"), "{error:#}");
        assert!(!restoring_dir_for(&data).exists());
        assert!(!restore_journal_path(&data).exists());
        assert_eq!(
            nestweaver_store::GraphStore::open_read_only(&data.join("brain.lbug"))
                .unwrap()
                .data_instance_id()
                .unwrap()
                .as_deref(),
            Some("captured-original")
        );
        backup_restore(&RestoreConfig {
            snapshot_path: snapshot,
            data_dir: data.clone(),
            replace: true,
        })
        .unwrap();
        assert_eq!(
            nestweaver_store::GraphStore::open_read_only(&data.join("brain.lbug"))
                .unwrap()
                .data_instance_id()
                .unwrap()
                .as_deref(),
            Some("default")
        );
    }

    #[test]
    fn absent_target_marker_interruptions_retry_without_unbound_destination_debris() {
        for force_copy in [false, true] {
            for crash_at in ["before-marker-rebind", "after-marker-rebind"] {
                let tmp = tempfile::tempdir().unwrap();
                let (data, snapshot, _) = seed_marker_replay_dataset(tmp.path(), "brain.lbug");
                std::fs::remove_dir_all(&data).unwrap();
                let output =
                    run_restore_in_child_copy_mode(&snapshot, &data, crash_at, &[], force_copy);
                assert_eq!(
                    output.status.code(),
                    Some(9),
                    "{crash_at}, copy={force_copy}: {}",
                    String::from_utf8_lossy(&output.stderr)
                );
                assert!(!restoring_dir_for(&data).exists());
                for _ in 0..2 {
                    recover_interrupted_restore(&data).unwrap();
                }
                assert!(
                    !data.exists(),
                    "unvalidated destination is rebuilt from the archive"
                );
                backup_restore(&RestoreConfig {
                    snapshot_path: snapshot,
                    data_dir: data.clone(),
                    replace: true,
                })
                .unwrap();
                assert!(
                    nestweaver_store::engine_format::has_current_file_marker(
                        &data.join("brain.lbug")
                    )
                    .unwrap()
                );
            }
        }
    }

    /// A cutover that copied every byte and STILL does not match the archive
    /// must not be published as complete. The rollback runs under the phase
    /// that says the destination is not authoritative, so a death during the
    /// rollback recovers too — and after it, the user's own pre-restore data
    /// is back and a clean retry succeeds.
    #[test]
    fn a_cutover_that_fails_validation_rolls_back_to_the_pre_restore_data() {
        let tmp = tempfile::tempdir().unwrap();

        let source_db = tmp.path().join("source").join("test.lbug");
        std::fs::create_dir_all(source_db.parent().unwrap()).unwrap();
        drop(nestweaver_store::GraphStore::create(&source_db).unwrap());
        let snapshot = tmp.path().join("test.nwsnap.zst");
        backup_save(&BackupConfig {
            db_path: source_db,
            output_path: snapshot.clone(),
            include_clones: false,
            instance_id: "validation-failure".to_string(),
            workspace_path: None,
            overwrite: false,
        })
        .unwrap();

        let data_dir = tmp.path().join("data");
        std::fs::create_dir_all(&data_dir).unwrap();
        let incumbent = nestweaver_store::GraphStore::create(&data_dir.join("test.lbug")).unwrap();
        incumbent
            .ensure_data_instance_id("incumbent-restore-fixture")
            .unwrap();
        drop(incumbent);

        // "crash-at" is deliberately a phase name that never fires: this run
        // must reach validation and refuse there, not die on the way.
        let output = run_restore_in_child_with_output(
            &snapshot,
            &data_dir,
            "never",
            &[("NW_RESTORE_TEST_CORRUPT_AFTER_COPY", "1")],
        );
        let diagnostics = String::from_utf8_lossy(&output.stderr);
        assert_eq!(
            output.status.code(),
            Some(3),
            "the restore must have REFUSED the corrupt cutover, not crashed or succeeded"
        );

        let corruption_witness = format!(
            "test injected graph corruption into {}",
            data_dir.join("test.lbug").display()
        );
        assert!(
            diagnostics.contains(corruption_witness.as_str()),
            "corruption injection was not reached: {diagnostics}"
        );
        assert!(
            diagnostics.contains("restored test.lbug does not match the archive: expected"),
            "checksum validation did not reject the copied graph: {diagnostics}"
        );
        assert!(
            diagnostics.contains(
                "failed validation after the cutover; the pre-restore data has been reinstated"
            ),
            "post-cutover rollback did not complete: {diagnostics}"
        );
        let preserved: Vec<_> = std::fs::read_dir(tmp.path())
            .unwrap()
            .map(Result::unwrap)
            .map(|entry| entry.path())
            .filter(|path| {
                path.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with("data.preserved-")
            })
            .collect();
        assert_eq!(preserved.len(), 1, "rollback must retain its unproven copy");
        assert!(
            diagnostics.contains(&format!(
                "unproven restore copy preserved at {}",
                preserved[0].display()
            )),
            "rollback must disclose the exact retained path: {diagnostics}"
        );
        assert_eq!(
            std::fs::read(preserved[0].join("test.lbug")).unwrap(),
            b"corrupted by the test"
        );

        // The rollback already ran inside the child; recovery is idempotent
        // over it, twice.
        for pass in 0..2 {
            recover_interrupted_restore(&data_dir)
                .unwrap_or_else(|error| panic!("pass {pass}: {error}"));
            assert_eq!(
                nestweaver_store::GraphStore::open_read_only(&data_dir.join("test.lbug"))
                    .unwrap()
                    .data_instance_id()
                    .unwrap()
                    .as_deref(),
                Some("incumbent-restore-fixture"),
                "pass {pass}: a failed validation must leave the pre-restore data in place"
            );
            assert!(
                nestweaver_store::GraphStore::open_read_only(&data_dir.join("test.lbug")).is_ok(),
                "pass {pass}: the reinstated dataset must open"
            );
            assert!(!restoring_dir_for(&data_dir).exists(), "pass {pass}");
        }

        // And a clean retry, with nothing injected, still works.
        backup_restore(&RestoreConfig {
            snapshot_path: snapshot,
            data_dir: data_dir.clone(),
            replace: true,
        })
        .expect("the retry after a rolled-back validation failure must succeed");
        assert!(nestweaver_store::GraphStore::open_read_only(&data_dir.join("test.lbug")).is_ok());
        assert!(!restore_journal_path(&data_dir).exists());
    }

    /// The absent-target counterweight, end to end: restoring into a directory
    /// that does not exist writes no journal residue and needs no recovery.
    #[test]
    fn restoring_into_an_absent_target_leaves_no_journal_or_aside_copy() {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("test.lbug");
        drop(nestweaver_store::GraphStore::create(&db_path).unwrap());
        let snapshot = tmp.path().join("test.nwsnap.zst");
        backup_save(&BackupConfig {
            db_path,
            output_path: snapshot.clone(),
            include_clones: false,
            instance_id: "absent-target".to_string(),
            workspace_path: None,
            overwrite: false,
        })
        .unwrap();

        let data_dir = tmp.path().join("fresh");
        backup_restore(&RestoreConfig {
            snapshot_path: snapshot,
            data_dir: data_dir.clone(),
            replace: true,
        })
        .unwrap();

        assert!(nestweaver_store::GraphStore::open_read_only(&data_dir.join("test.lbug")).is_ok());
        assert!(!restoring_dir_for(&data_dir).exists());
        assert!(!restore_journal_path(&data_dir).exists());
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
            overwrite: false,
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
            overwrite: false,
        };

        backup_save(&config).unwrap();

        let restore_dir = dir.path().join("restored");
        let restore_config = RestoreConfig {
            snapshot_path: output,
            data_dir: restore_dir.clone(),
            replace: true,
        };

        let result = backup_restore(&restore_config).unwrap();
        assert_eq!(result.manifest.instance_id, "default");

        let restored_db = restore_dir.join("test.lbug");
        let store = nestweaver_store::GraphStore::open_read_only(&restored_db).unwrap();
        assert!(store.db_path().is_some());
    }

    #[test]
    fn multiple_graph_inventory_is_refused_before_restore_cutover() {
        let tmp = tempfile::tempdir().unwrap();
        let (data, snapshot, _) = seed_marker_replay_dataset(tmp.path(), "brain.lbug");
        let before_graph = sha256_stream_path(data.join("brain.lbug")).unwrap();
        let marker = nestweaver_store::engine_format::sidecar_path(&data.join("brain.lbug"));
        let before_marker = std::fs::read(&marker).unwrap();
        let staging = tempfile::tempdir_in(tmp.path()).unwrap();
        let decoder =
            nestweaver_store::zstd::Decoder::new(std::fs::File::open(&snapshot).unwrap()).unwrap();
        tar::Archive::new(decoder).unpack(staging.path()).unwrap();
        let publication_path = staging
            .path()
            .join(crate::publication::PUBLICATION_MANIFEST_FILE);
        let mut publication: crate::publication::PublicationBundleV3 =
            serde_json::from_slice(&std::fs::read(&publication_path).unwrap()).unwrap();
        let mut second = publication
            .artifacts
            .iter()
            .find(|artifact| artifact.kind == crate::publication::ArtifactKind::Graph)
            .unwrap()
            .clone();
        std::fs::copy(
            staging.path().join(&second.path),
            staging.path().join("second.lbug"),
        )
        .unwrap();
        second.path = "second.lbug".into();
        publication.artifacts.push(second);
        let publication_bytes = serde_json::to_vec_pretty(&publication).unwrap();
        std::fs::write(&publication_path, &publication_bytes).unwrap();
        let manifest_path = staging.path().join("manifest.json");
        let mut manifest: BackupManifest =
            serde_json::from_slice(&std::fs::read(&manifest_path).unwrap()).unwrap();
        manifest.checksums.insert(
            "second.lbug".into(),
            sha256_stream_path(staging.path().join("second.lbug")).unwrap(),
        );
        manifest.checksums.insert(
            crate::publication::PUBLICATION_MANIFEST_FILE.into(),
            sha256_stream_path(&publication_path).unwrap(),
        );
        manifest.publication_manifest_blake3 = crate::hash::blake3_hex_bytes(&publication_bytes);
        std::fs::write(
            &manifest_path,
            serde_json::to_vec_pretty(&manifest).unwrap(),
        )
        .unwrap();
        // All content hashes are consistent: rejection must be about graph
        // cardinality, before any destructive phase or marker mutation.
        verify_backup_checksums(staging.path(), &manifest).unwrap();
        let forged = tmp.path().join("multiple.nwsnap.zst");
        package_tar_zstd(staging.path(), &forged).unwrap();
        let inspected = backup_inspect(&forged).unwrap_err();
        assert!(
            format!("{inspected:#}").contains("exactly one graph artifact"),
            "{inspected:#}"
        );
        let refused = backup_restore(&RestoreConfig {
            snapshot_path: forged,
            data_dir: data.clone(),
            replace: true,
        })
        .unwrap_err();
        assert!(
            format!("{refused:#}").contains("exactly one graph artifact"),
            "{refused:#}"
        );
        assert_eq!(
            sha256_stream_path(data.join("brain.lbug")).unwrap(),
            before_graph
        );
        assert_eq!(std::fs::read(marker).unwrap(), before_marker);
        assert!(!restore_journal_path(&data).exists());
        assert!(!restoring_dir_for(&data).exists());
    }

    #[test]
    fn held_custom_basename_and_kuzu_writers_refuse_restore_without_touching_incumbents() {
        for graph in ["graph.db", "legacy.kuzu"] {
            let tmp = tempfile::tempdir().unwrap();
            let (data, snapshot, _) = seed_marker_replay_dataset(tmp.path(), graph);
            let db = data.join(graph);
            assert!(
                restore_lease_targets(&data)
                    .unwrap()
                    .contains(&nestweaver_store::canonical_db_path(&db))
            );
            let before_graph = sha256_stream_path(&db).unwrap();
            let marker = nestweaver_store::engine_format::sidecar_path(&db);
            let before_marker = std::fs::read(&marker).unwrap();
            let writer = nestweaver_store::acquire_db_write_lease(&db).unwrap();
            let error = backup_restore(&RestoreConfig {
                snapshot_path: snapshot.clone(),
                data_dir: data.clone(),
                replace: true,
            })
            .unwrap_err();
            assert!(
                format!("{error:#}").contains("write lease"),
                "{graph}: {error:#}"
            );
            assert_eq!(sha256_stream_path(&db).unwrap(), before_graph);
            assert_eq!(std::fs::read(&marker).unwrap(), before_marker);
            assert!(!restore_journal_path(&data).exists());
            assert!(!restoring_dir_for(&data).exists());
            drop(writer);
            backup_restore(&RestoreConfig {
                snapshot_path: snapshot,
                data_dir: data,
                replace: true,
            })
            .unwrap();
        }
    }

    /// Rewrite a current archive into the shape a pre-cutover release wrote:
    /// bundle format 3, a graph with no engine-format marker and no
    /// `.engine-format` artifact, every checksum and descriptor recomputed so
    /// the archive is internally consistent. Returns the legacy graph's bytes.
    fn rewrite_as_pre_cutover(archive: &Path, out: &Path) -> Vec<u8> {
        let staging = tempfile::tempdir().unwrap();
        let decoder =
            nestweaver_store::zstd::Decoder::new(std::fs::File::open(archive).unwrap()).unwrap();
        tar::Archive::new(decoder).unpack(staging.path()).unwrap();
        let mut manifest: BackupManifest =
            serde_json::from_slice(&std::fs::read(staging.path().join("manifest.json")).unwrap())
                .unwrap();
        let publication_path = staging
            .path()
            .join(crate::publication::PUBLICATION_MANIFEST_FILE);
        let mut publication: crate::publication::PublicationBundleV3 =
            serde_json::from_slice(&std::fs::read(&publication_path).unwrap()).unwrap();
        let graph_rel = publication
            .artifacts
            .iter()
            .find(|artifact| artifact.kind == crate::publication::ArtifactKind::Graph)
            .unwrap()
            .path
            .clone();
        let graph = staging.path().join(&graph_rel);
        let before: std::collections::BTreeSet<_> = walkdir::WalkDir::new(staging.path())
            .into_iter()
            .flatten()
            .map(|entry| entry.path().to_path_buf())
            .collect();
        {
            let store = nestweaver_store::GraphStore::open(&graph).unwrap();
            let conn = store.begin_transaction().unwrap();
            conn.query(&format!(
                "MATCH (m:Meta) WHERE m.key = '{}' DELETE m",
                nestweaver_store::engine_format::ENGINE_FORMAT_META_KEY
            ))
            .unwrap();
            store.commit_transaction(&conn).unwrap();
            drop(conn);
            store.checkpoint().unwrap();
        }
        // Drop the sidecar and anything the edit itself created.
        let sidecar_rel = format!(
            "{graph_rel}{}",
            nestweaver_store::engine_format::ENGINE_FORMAT_SIDECAR_SUFFIX
        );
        for entry in walkdir::WalkDir::new(staging.path()).into_iter().flatten() {
            if entry.file_type().is_file() && !before.contains(entry.path()) {
                std::fs::remove_file(entry.path()).unwrap();
            }
        }
        std::fs::remove_file(staging.path().join(&sidecar_rel)).unwrap();
        publication
            .artifacts
            .retain(|artifact| artifact.path != sidecar_rel);
        let (size, digest) = crate::hash::blake3_file(&graph).unwrap();
        for artifact in &mut publication.artifacts {
            if artifact.path == graph_rel {
                artifact.byte_size = size;
                artifact.blake3 = digest.clone();
            }
        }
        publication.format_version = crate::snapshot::LEGACY_ENGINE_SNAPSHOT_FORMAT_VERSION;
        let publication_bytes = serde_json::to_vec_pretty(&publication).unwrap();
        std::fs::write(&publication_path, &publication_bytes).unwrap();
        manifest.publication_manifest_blake3 = crate::hash::blake3_hex_bytes(&publication_bytes);
        manifest.checksums.remove(&sidecar_rel);
        for (path, checksum) in manifest.checksums.iter_mut() {
            *checksum = sha256_stream_path(staging.path().join(path)).unwrap();
        }
        std::fs::write(
            staging.path().join("manifest.json"),
            serde_json::to_vec_pretty(&manifest).unwrap(),
        )
        .unwrap();
        package_tar_zstd(staging.path(), out).unwrap();
        std::fs::read(&graph).unwrap()
    }

    /// A pre-cutover archive is the operator's data and their rollback path:
    /// it restores byte-for-byte, the restore WARNS that the result must be
    /// rebuilt, and the restored database is then refused, not opened.
    #[test]
    fn a_pre_cutover_archive_restores_unchanged_and_warns() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.lbug");
        drop(nestweaver_store::GraphStore::create(&db_path).unwrap());
        let current = dir.path().join("current.nwsnap.zst");
        backup_save(&BackupConfig {
            db_path: db_path.clone(),
            output_path: current.clone(),
            include_clones: false,
            instance_id: "test".to_string(),
            workspace_path: None,
            overwrite: false,
        })
        .unwrap();

        // Counterweight: a current archive restores without the warning.
        let fresh = backup_restore(&RestoreConfig {
            snapshot_path: current.clone(),
            data_dir: dir.path().join("fresh"),
            replace: true,
        })
        .unwrap();
        assert!(
            !fresh
                .manifest
                .warnings
                .iter()
                .any(|warning| warning.contains("LadybugDB 0.21")),
            "{:?}",
            fresh.manifest.warnings
        );

        let legacy = dir.path().join("legacy.nwsnap.zst");
        let legacy_graph = rewrite_as_pre_cutover(&current, &legacy);
        let restore_dir = dir.path().join("restored");
        let result = backup_restore(&RestoreConfig {
            snapshot_path: legacy,
            data_dir: restore_dir.clone(),
            replace: true,
        })
        .expect("a pre-cutover archive must still restore");
        let warning = result
            .manifest
            .warnings
            .iter()
            .find(|warning| warning.contains("LadybugDB 0.21"))
            .unwrap_or_else(|| panic!("no cutover warning: {:?}", result.manifest.warnings));
        assert!(
            warning.contains("nestweaver publication rebuild"),
            "{warning}"
        );
        assert!(
            warning.contains("nestweaver::db_rebuild_required"),
            "{warning}"
        );

        let restored = restore_dir.join("test.lbug");
        assert_eq!(std::fs::read(&restored).unwrap(), legacy_graph);
        assert!(
            nestweaver_store::GraphStore::open(&restored)
                .err()
                .expect("the restored pre-cutover graph must be refused")
                .is_rebuild_required()
        );
    }

    /// A restored database is a NEW file; the restore re-binds its
    /// engine-format sidecar to it, so if it later crashes with a log beside
    /// it, it still recovers instead of being refused as unverifiable.
    #[test]
    fn a_restored_database_that_crashes_still_recovers() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.lbug");
        drop(nestweaver_store::GraphStore::create(&db_path).unwrap());
        let archive = dir.path().join("a.nwsnap.zst");
        backup_save(&BackupConfig {
            db_path: db_path.clone(),
            output_path: archive.clone(),
            include_clones: false,
            instance_id: "test".to_string(),
            workspace_path: None,
            overwrite: false,
        })
        .unwrap();
        let restore_dir = dir.path().join("restored");
        backup_restore(&RestoreConfig {
            snapshot_path: archive,
            data_dir: restore_dir.clone(),
            replace: true,
        })
        .unwrap();
        let restored = restore_dir.join("test.lbug");
        std::fs::write(restore_dir.join("test.lbug.wal.checkpoint"), b"").unwrap();
        drop(
            nestweaver_store::GraphStore::open(&restored)
                .expect("a restored database with crash debris must recover"),
        );
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
                overwrite: false,
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
            overwrite: false,
        };
        backup_save(&config).unwrap();

        let restore_dir = dir.path().join("restored");
        let restore_config = RestoreConfig {
            snapshot_path: output,
            data_dir: restore_dir.clone(),
            replace: true,
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
            overwrite: false,
        };

        let result = backup_save(&config).unwrap();
        assert!(output.exists());
        assert_eq!(result.manifest.instance_id, "default");
    }

    #[test]
    fn backup_sidecars_include_incomplete_extension_migration_journal() {
        assert!(SIDECAR_SUFFIXES.contains(&".extensions.migration.json"));
        assert!(SIDECAR_SUFFIXES.contains(&".extensions.handoff.json"));
        assert!(
            SIDECAR_SUFFIXES.contains(&crate::resolver_generation::RESOLVER_GENERATION_SIDECAR)
        );
        assert!(SIDECAR_SUFFIXES.contains(&crate::code_links::CODE_LINKS_SIDECAR));
    }

    /// nw-670 review L7: `<db>.code_links.json` (the link-rules version and
    /// owed-link debt) is a compatibility stamp that travels with the graph.
    /// ba906d10 listed it for backup without a contract, so any backup of a
    /// brain the reconciler had touched was refused as "unclassified", and a
    /// publication slot holding it could not be sealed. Both must now carry
    /// it, described.
    #[test]
    fn the_code_links_stamp_is_backed_up_and_sealed_with_the_graph() {
        let dir = tempfile::tempdir().unwrap();
        let slot = dir.path().join("slot");
        std::fs::create_dir(&slot).unwrap();
        let db_path = slot.join(crate::publication::PUBLICATION_GRAPH_FILE);
        drop(nestweaver_store::GraphStore::create(&db_path).unwrap());
        crate::code_links::mark_code_links_pending(&db_path, "test");
        let stamp = format!(
            "{}{}",
            crate::publication::PUBLICATION_GRAPH_FILE,
            crate::code_links::CODE_LINKS_SIDECAR
        );

        let bundle = seal_publication_slot(&db_path, &slot)
            .expect("a slot holding the code-links stamp must seal");
        assert!(
            bundle.artifacts.iter().any(|a| a.path == stamp
                && a.kind == crate::publication::ArtifactKind::CompatibilityStamp)
        );

        // Then an archive backup (which leaves the writer anchor behind).
        let output = dir.path().join("b.nwsnap.zst");
        let backup = backup_save(&BackupConfig {
            db_path: db_path.clone(),
            output_path: output,
            include_clones: false,
            instance_id: "default".to_string(),
            workspace_path: None,
            overwrite: false,
        })
        .expect("a backup must accept the code-links stamp");
        assert!(
            backup
                .manifest
                .checksums
                .keys()
                .any(|path| path.ends_with(&stamp)),
            "{:?}",
            backup.manifest.checksums.keys().collect::<Vec<_>>()
        );
    }

    /// A rebuilt slot that indexed a vault holds the vault-registration
    /// record and the skipped-notes disclosure; the seal must describe both
    /// instead of refusing them as unclassified.
    #[test]
    fn a_slot_holding_vault_index_sidecars_seals() {
        let dir = tempfile::tempdir().unwrap();
        let slot = dir.path().join("slot");
        std::fs::create_dir(&slot).unwrap();
        let db_path = slot.join(crate::publication::PUBLICATION_GRAPH_FILE);
        drop(nestweaver_store::GraphStore::create(&db_path).unwrap());
        let registrations = format!(
            "{}{}",
            crate::publication::PUBLICATION_GRAPH_FILE,
            crate::vault_registration::VAULT_REGISTRATIONS_SUFFIX
        );
        let skipped = format!(
            "{}{}",
            crate::publication::PUBLICATION_GRAPH_FILE,
            crate::index_md::SKIPPED_NOTES_SIDECAR_SUFFIX
        );
        let derivation = format!(
            "{}{}",
            crate::publication::PUBLICATION_GRAPH_FILE,
            crate::markdown_derivation::RECORD_SUFFIX
        );
        std::fs::write(slot.join(&registrations), b"{}").unwrap();
        std::fs::write(slot.join(&skipped), b"{}").unwrap();
        std::fs::write(slot.join(&derivation), b"{}").unwrap();

        let bundle = seal_publication_slot(&db_path, &slot)
            .expect("a slot holding vault index sidecars must seal");
        assert!(bundle.artifacts.iter().any(|a| a.path == registrations
            && a.kind == crate::publication::ArtifactKind::CompatibilityStamp));
        assert!(
            bundle
                .artifacts
                .iter()
                .any(|a| a.path == skipped
                    && a.kind == crate::publication::ArtifactKind::FileMetadata)
        );
        assert!(bundle.artifacts.iter().any(|a| a.path == derivation
            && a.kind == crate::publication::ArtifactKind::CompatibilityStamp));
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
    fn live_slot_lock_requires_exact_authority_and_is_never_archived() {
        let dir = tempfile::tempdir().unwrap();
        let slot = dir.path().join("slot");
        std::fs::create_dir(&slot).unwrap();
        let db = slot.join(crate::publication::PUBLICATION_GRAPH_FILE);
        drop(nestweaver_store::GraphStore::create(&db).unwrap());
        let authority = nestweaver_store::acquire_db_write_lease(&db).unwrap();
        let lock = nestweaver_store::write_lease_path(&db);
        assert!(lock.exists());
        assert!(
            seal_publication_slot(&db, &slot)
                .unwrap_err()
                .to_string()
                .contains("write.lock")
        );
        let bundle = seal_publication_slot_with_authority(&db, &slot, &authority).unwrap();
        assert!(lock.exists(), "sealing must retain the writer anchor");
        assert!(
            !bundle
                .artifacts
                .iter()
                .any(|a| a.path.ends_with("write.lock"))
        );
        crate::publication::validate_slot_contents(
            &slot,
            &bundle,
            crate::publication::ArtifactBytes::Verified,
        )
        .unwrap();
        std::fs::write(slot.join("foreign.write.lock"), "foreign").unwrap();
        assert!(
            crate::publication::validate_slot_contents(
                &slot,
                &bundle,
                crate::publication::ArtifactBytes::Verified
            )
            .is_err()
        );
        std::fs::remove_file(slot.join("foreign.write.lock")).unwrap();
        let other = dir.path().join("other.lbug");
        drop(nestweaver_store::GraphStore::create(&other).unwrap());
        let wrong = nestweaver_store::acquire_db_write_lease(&other).unwrap();
        assert!(seal_publication_slot_with_authority(&db, &slot, &wrong).is_err());
        drop(authority);
        crate::publication::validate_slot_contents(
            &slot,
            &bundle,
            crate::publication::ArtifactBytes::Live,
        )
        .unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn live_slot_refuses_symlinked_writer_anchor() {
        let dir = tempfile::tempdir().unwrap();
        let slot = dir.path().join("slot");
        std::fs::create_dir(&slot).unwrap();
        let db = slot.join(crate::publication::PUBLICATION_GRAPH_FILE);
        drop(nestweaver_store::GraphStore::create(&db).unwrap());
        let authority = nestweaver_store::acquire_db_write_lease(&db).unwrap();
        let bundle = seal_publication_slot_with_authority(&db, &slot, &authority).unwrap();
        let lock = nestweaver_store::write_lease_path(&db);
        // This scratch-only replacement simulates tampering after admission.
        std::fs::remove_file(&lock).unwrap();
        let outside = dir.path().join("outside.lock");
        std::fs::write(&outside, "outside").unwrap();
        std::os::unix::fs::symlink(&outside, &lock).unwrap();
        assert!(seal_publication_slot_with_authority(&db, &slot, &authority).is_err());
        assert!(
            crate::publication::validate_slot_contents(
                &slot,
                &bundle,
                crate::publication::ArtifactBytes::Verified
            )
            .is_err()
        );
    }

    #[test]
    fn legacy_backup_identity_requires_unique_graph_ownership() {
        let store = nestweaver_store::GraphStore::in_memory().unwrap();
        assert_eq!(backup_logical_instance_id(&store).unwrap(), "default");
        let vault = |instance: &str| nestweaver_schema::Vault {
            uid: format!("vault:{instance}:v"),
            name: "v".into(),
            root_path: "/fixture".into(),
            instance_id: instance.into(),
        };
        store.upsert_vault(&vault("legacy")).unwrap();
        assert_eq!(backup_logical_instance_id(&store).unwrap(), "legacy");
        assert_eq!(store.data_instance_id().unwrap(), None);
        store.upsert_vault(&vault("foreign")).unwrap();
        assert!(
            backup_logical_instance_id(&store)
                .unwrap_err()
                .to_string()
                .contains("multiple")
        );
    }

    #[test]
    fn backup_instance_is_graph_owned_even_when_path_and_config_disagree() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("arbitrary-name.lbug");
        let store = nestweaver_store::GraphStore::create(&db).unwrap();
        store.ensure_data_instance_id("logical-owner").unwrap();
        drop(store);
        let config = BackupConfig {
            db_path: db,
            output_path: dir.path().join("snapshot.nwsnap.zst"),
            include_clones: false,
            instance_id: "physical-path-hash".into(),
            workspace_path: None,
            overwrite: false,
        };
        let result = backup_save(&config).unwrap();
        assert_eq!(result.manifest.instance_id, "logical-owner");
        assert_eq!(
            backup_inspect(&config.output_path).unwrap().instance_id,
            "logical-owner"
        );
        let restored = dir.path().join("moved");
        backup_restore(&RestoreConfig {
            snapshot_path: config.output_path,
            data_dir: restored.clone(),
            replace: true,
        })
        .unwrap();
        let reopened =
            nestweaver_store::GraphStore::open_read_only(&restored.join("arbitrary-name.lbug"))
                .unwrap();
        assert_eq!(
            backup_logical_instance_id(&reopened).unwrap(),
            "logical-owner"
        );
    }

    #[test]
    fn restore_refuses_forged_logical_identity_before_touching_target() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("owned.lbug");
        let store = nestweaver_store::GraphStore::create(&db).unwrap();
        store.ensure_data_instance_id("owned").unwrap();
        drop(store);
        let original = dir.path().join("original.nwsnap.zst");
        backup_save(&BackupConfig {
            db_path: db,
            output_path: original.clone(),
            include_clones: false,
            instance_id: "wrong-config".into(),
            workspace_path: None,
            overwrite: false,
        })
        .unwrap();
        let extracted = dir.path().join("extracted");
        let decoder =
            nestweaver_store::zstd::Decoder::new(std::fs::File::open(original).unwrap()).unwrap();
        tar::Archive::new(decoder).unpack(&extracted).unwrap();
        let path = extracted.join("manifest.json");
        let mut manifest: BackupManifest =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        manifest.instance_id = "forged".into();
        std::fs::write(path, serde_json::to_vec(&manifest).unwrap()).unwrap();
        let forged = dir.path().join("forged.nwsnap.zst");
        package_tar_zstd(&extracted, &forged).unwrap();
        let target = dir.path().join("existing");
        std::fs::create_dir(&target).unwrap();
        // A NestWeaver data directory, so the identity check (not the
        // foreign-directory guard) is what refuses.
        let incumbent = nestweaver_store::GraphStore::create(&target.join("prior.lbug")).unwrap();
        incumbent
            .ensure_data_instance_id("forgery-target-fixture")
            .unwrap();
        drop(incumbent);
        let error = backup_restore(&RestoreConfig {
            snapshot_path: forged,
            data_dir: target.clone(),
            replace: true,
        })
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("logical instance identity mismatch"),
            "{error:#}"
        );
        assert_eq!(
            nestweaver_store::GraphStore::open_read_only(&target.join("prior.lbug"))
                .unwrap()
                .data_instance_id()
                .unwrap()
                .as_deref(),
            Some("forgery-target-fixture")
        );
        assert!(!target.join("owned.lbug").exists());
    }

    #[test]
    fn test_schema_compatibility_rejects_newer() {
        let manifest = BackupManifest {
            warnings: Vec::new(),
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

    // -------------------------------------------------------------------
    // nw-375: `backup_restore` takes its own exclusive restore authority.
    // -------------------------------------------------------------------

    /// Re-invoked as a CHILD PROCESS by
    /// [`backup_restore_refuses_while_another_process_holds_the_target_write_lease`];
    /// a no-op in a normal run. Holds a REAL cross-process write lease on the
    /// named database, signals readiness by creating a file, then waits for a
    /// release signal before exiting — so the parent's `backup_restore` call
    /// races a genuinely external holder, not an in-process fake.
    #[test]
    fn restore_lease_holder_child() {
        let (Ok(db), Ok(ready_file), Ok(release_file)) = (
            std::env::var("NW_HOLD_LEASE_DB"),
            std::env::var("NW_HOLD_LEASE_READY_FILE"),
            std::env::var("NW_HOLD_LEASE_RELEASE_FILE"),
        ) else {
            return;
        };
        let authority = nestweaver_store::acquire_db_write_lease(Path::new(&db))
            .expect("holder child must acquire the write lease");
        std::fs::write(&ready_file, b"ready").expect("signal readiness");
        let release_file = PathBuf::from(release_file);
        let deadline = Instant::now() + Duration::from_secs(10);
        while !release_file.exists() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        drop(authority);
    }

    fn spawn_restore_lease_holder(
        db: &Path,
        ready_file: &Path,
        release_file: &Path,
    ) -> std::process::Child {
        std::process::Command::new(std::env::current_exe().expect("this test binary"))
            .args([
                "--exact",
                "--nocapture",
                "--test-threads=1",
                "backup::tests::restore_lease_holder_child",
            ])
            .env("NW_HOLD_LEASE_DB", db)
            .env("NW_HOLD_LEASE_READY_FILE", ready_file)
            .env("NW_HOLD_LEASE_RELEASE_FILE", release_file)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn the lease-holder child")
    }

    fn wait_for_file(path: &Path, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if path.exists() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        path.exists()
    }

    /// THE CORE nw-375 CLAIM, demonstrated rather than only asserted:
    /// `nestweaver_engine::backup_restore` is `pub` and previously took no
    /// lease at all, so a caller reaching the engine directly — skipping the
    /// CLI's `with_exclusive_restore_access` choke point entirely — bypassed
    /// exclusion. This calls `backup_restore` exactly that way: bare, with no
    /// CLI wrapper anywhere in the call stack. A REAL other process holds the
    /// target database's write lease throughout. The invariant under test is
    /// that `backup_restore` now establishes its own authority by
    /// construction, so this refusal cannot be skipped by a caller that
    /// forgets or declines to take the lease itself.
    #[test]
    fn backup_restore_refuses_while_another_process_holds_the_target_write_lease() {
        let tmp = tempfile::tempdir().unwrap();

        let source_db = tmp.path().join("source").join("test.lbug");
        std::fs::create_dir_all(source_db.parent().unwrap()).unwrap();
        drop(nestweaver_store::GraphStore::create(&source_db).unwrap());
        let snapshot = tmp.path().join("test.nwsnap.zst");
        backup_save(&BackupConfig {
            db_path: source_db,
            output_path: snapshot.clone(),
            include_clones: false,
            instance_id: "lease-race".to_string(),
            workspace_path: None,
            overwrite: false,
        })
        .unwrap();

        let data_dir = tmp.path().join("data");
        std::fs::create_dir_all(&data_dir).unwrap();
        let target_db = data_dir.join("test.lbug");
        drop(nestweaver_store::GraphStore::create(&target_db).unwrap());
        std::fs::write(data_dir.join("INCUMBENT"), b"the user's own data").unwrap();

        let ready_file = tmp.path().join("holder-ready");
        let release_file = tmp.path().join("holder-release");
        let mut holder = spawn_restore_lease_holder(&target_db, &ready_file, &release_file);
        assert!(
            wait_for_file(&ready_file, Duration::from_secs(5)),
            "the external holder must signal it acquired the write lease"
        );

        let error = backup_restore(&RestoreConfig {
            snapshot_path: snapshot,
            data_dir: data_dir.clone(),
            replace: true,
        })
        .expect_err(
            "a bare engine call must be refused while another process holds the target's \
             write lease — this is the nw-375 invariant, not the CLI's",
        );
        // The external holder's plain `acquire_db_write_lease` also takes a
        // SHARED namespace lock as part of its own acquisition (see
        // `acquire_db_write_lease_inner`), so `backup_restore`'s EXCLUSIVE
        // namespace acquisition is the first thing to conflict and refuse —
        // it need not reach the per-database lease step to be excluded.
        // Either failure point is a correct refusal; assert the message names
        // a concrete path and an actionable remedy either way.
        let message = error.to_string();
        assert!(
            message.contains("write lease") && message.contains(&data_dir.display().to_string()),
            "the refusal must name the write lease and a concrete path, not just fail \
             silently: {message}"
        );
        assert!(
            message.contains("stop every writer and retry")
                || message.contains("Stop the holder first, then retry"),
            "the refusal must name a remedy the operator can execute: {message}"
        );

        std::fs::write(&release_file, b"go").expect("signal release");
        let status = holder.wait().expect("holder child exits");
        assert!(status.success(), "holder child must exit cleanly");

        // Exclusion, not just an error message: the incumbent data must be
        // completely untouched by the refused attempt.
        assert!(
            data_dir.join("INCUMBENT").exists(),
            "a refused restore must not have touched the incumbent data at all"
        );
        assert!(
            nestweaver_store::GraphStore::open_read_only(&target_db).is_ok(),
            "the incumbent database must remain exactly as it was"
        );
    }

    /// The other half: a caller that ALREADY holds the exact authority
    /// `backup_restore` would otherwise take itself — exactly what the CLI's
    /// `with_exclusive_restore_access` choke point does before calling in —
    /// must not be fought. Re-acquiring here would self-conflict (the
    /// underlying primitives are scoped to the open file description / a
    /// process-local claim registry, not "another equivalent call in this
    /// process already has it"), which would break the one caller that
    /// already does this correctly.
    #[test]
    fn backup_restore_recognizes_an_already_held_outer_authority_and_proceeds() {
        let tmp = tempfile::tempdir().unwrap();

        let source_db = tmp.path().join("source").join("test.lbug");
        std::fs::create_dir_all(source_db.parent().unwrap()).unwrap();
        drop(nestweaver_store::GraphStore::create(&source_db).unwrap());
        let snapshot = tmp.path().join("test.nwsnap.zst");
        backup_save(&BackupConfig {
            db_path: source_db,
            output_path: snapshot.clone(),
            include_clones: false,
            instance_id: "outer-authority".to_string(),
            workspace_path: None,
            overwrite: false,
        })
        .unwrap();

        let data_dir = tmp.path().join("data");

        // Exactly what the CLI choke point does before calling in: acquire
        // the namespace authority for the target and its `.restoring`
        // sibling, held across the call.
        let namespace = nestweaver_store::acquire_db_namespace_lease(&data_dir).unwrap();
        let restoring_namespace =
            nestweaver_store::acquire_db_namespace_lease(&restoring_dir_for(&data_dir)).unwrap();
        assert!(nestweaver_store::current_process_claims_namespace_lease(
            &data_dir
        ));

        backup_restore(&RestoreConfig {
            snapshot_path: snapshot,
            data_dir: data_dir.clone(),
            replace: true,
        })
        .expect(
            "a caller that already holds the outer authority must not be fought by \
             backup_restore's own acquisition",
        );

        drop(namespace);
        drop(restoring_namespace);
        assert!(nestweaver_store::GraphStore::open_read_only(&data_dir.join("test.lbug")).is_ok());
    }

    /// THE HIGH-SEVERITY GAP THE PREVIOUS TEST COULD NOT SEE: it exercised
    /// `Borrowed` against a `data_dir` with NOTHING in it, so verifying the
    /// per-database half of the authority was vacuously satisfied either way
    /// and the gap was unobservable. Here `data_dir` holds a REAL incumbent
    /// database, and the namespace lease is taken by a caller that has
    /// nothing to do with restoring anything — it never took the incumbent's
    /// write lease at all. `current_process_claims_namespace_lease` alone
    /// must NOT be enough to license `Borrowed`: without verifying every
    /// enumerated target is ALSO a write lease this process holds,
    /// `backup_restore` would destroy the incumbent database with no
    /// exclusion over it whatsoever — worse than refusing, because nothing
    /// stops a genuine writer from racing the destructive rename underneath
    /// it.
    #[test]
    fn backup_restore_does_not_trust_a_namespace_only_claim_over_a_real_incumbent() {
        let tmp = tempfile::tempdir().unwrap();

        let source_db = tmp.path().join("source").join("test.lbug");
        std::fs::create_dir_all(source_db.parent().unwrap()).unwrap();
        drop(nestweaver_store::GraphStore::create(&source_db).unwrap());
        let snapshot = tmp.path().join("test.nwsnap.zst");
        backup_save(&BackupConfig {
            db_path: source_db,
            output_path: snapshot.clone(),
            include_clones: false,
            instance_id: "namespace-only-claim".to_string(),
            workspace_path: None,
            overwrite: false,
        })
        .unwrap();

        // A REAL incumbent database `backup_restore` is about to destroy.
        let data_dir = tmp.path().join("data");
        std::fs::create_dir_all(&data_dir).unwrap();
        let target_db = data_dir.join("test.lbug");
        drop(nestweaver_store::GraphStore::create(&target_db).unwrap());
        std::fs::write(data_dir.join("INCUMBENT"), b"the user's own data").unwrap();

        // An UNRELATED in-process caller takes only the namespace lease —
        // via the same public API the CLI choke point uses, but for a
        // reason that has nothing to do with this restore, and critically
        // WITHOUT ever taking the incumbent's write lease. This is exactly
        // what `current_process_claims_namespace_lease` alone cannot tell
        // apart from a caller that took the full, verified authority.
        let namespace = nestweaver_store::acquire_db_namespace_lease(&data_dir).unwrap();
        let restoring_namespace =
            nestweaver_store::acquire_db_namespace_lease(&restoring_dir_for(&data_dir)).unwrap();
        assert!(nestweaver_store::current_process_claims_namespace_lease(
            &data_dir
        ));
        assert!(
            !nestweaver_store::current_process_claims_write_lease(&target_db),
            "the setup must not itself hold the incumbent's write lease — that is the point"
        );

        let error = backup_restore(&RestoreConfig {
            snapshot_path: snapshot,
            data_dir: data_dir.clone(),
            replace: true,
        })
        .expect_err(
            "a namespace-only claim must not license Borrowed over a real, unexcluded \
             incumbent database",
        );
        let message = error.to_string();
        assert!(
            message.contains("write lease") || message.contains("namespace"),
            "the refusal must explain why, not fail silently: {message}"
        );

        drop(namespace);
        drop(restoring_namespace);

        // Exclusion, not just an error: the incumbent must be completely
        // untouched.
        assert!(
            data_dir.join("INCUMBENT").exists(),
            "a refused restore must not have touched the unexcluded incumbent at all"
        );
        assert!(
            nestweaver_store::GraphStore::open_read_only(&target_db).is_ok(),
            "the incumbent database must remain exactly as it was"
        );
    }
}

#[cfg(test)]
mod hardening_statistics_tests {
    use super::*;
    struct Fault(usize);
    impl BackupStatistics for Fault {
        fn count(&self, store: &nestweaver_store::GraphStore) -> anyhow::Result<usize> {
            anyhow::ensure!(self.0 != 0, "injected count failure");
            StoreBackupStatistics.count(store)
        }
        fn per_repo(
            &self,
            store: &nestweaver_store::GraphStore,
        ) -> anyhow::Result<HashMap<String, usize>> {
            anyhow::ensure!(self.0 != 1, "injected per-repo failure");
            StoreBackupStatistics.per_repo(store)
        }
        fn repos(
            &self,
            store: &nestweaver_store::GraphStore,
        ) -> anyhow::Result<Vec<nestweaver_schema::Repo>> {
            anyhow::ensure!(self.0 != 2, "injected inventory failure");
            StoreBackupStatistics.repos(store)
        }
    }
    #[test]
    fn no_archive_can_publish_when_any_statistics_read_fails() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("stats.lbug");
        let store = nestweaver_store::GraphStore::create(&db).unwrap();
        let config = BackupConfig {
            db_path: db,
            output_path: dir.path().join("backup.zst"),
            include_clones: false,
            instance_id: "default".into(),
            workspace_path: None,
            overwrite: false,
        };
        for stage in 0..3 {
            let result = stage_backup_with_statistics(&store, &config, &Fault(stage));
            assert!(result.is_err(), "stage {stage} must refuse");
            assert!(!config.output_path.exists());
        }
        let staged = stage_backup_from_store(&store, &config).unwrap();
        let result = package_staged(&config, staged).unwrap();
        assert_eq!(result.manifest.symbol_count, 0);
        assert!(config.output_path.exists());
    }
}

#[cfg(test)]
mod destination_guard_tests {
    use super::*;

    #[test]
    fn output_protects_another_database_and_its_aliases() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source.lbug");
        let other = dir.path().join("other.lbug");
        std::fs::write(&source, b"LBUGsource").unwrap();
        std::fs::write(&other, b"LBUGother").unwrap();
        let mut targets = vec![
            other.clone(),
            dir.path().join("new.kuzu"),
            dir.path().join("other.lbug.wal"),
        ];
        #[cfg(unix)]
        {
            let alias = dir.path().join("archive.zst");
            std::fs::hard_link(&other, &alias).unwrap();
            targets.push(alias);
            let link = dir.path().join("linked.zst");
            std::os::unix::fs::symlink(&other, &link).unwrap();
            targets.push(link);
        }
        for target in targets {
            let error = refuse_output_in_database_footprint(&source, &target).unwrap_err();
            assert!(error.downcast_ref::<BackupRefusal>().is_some(), "{error:#}");
        }
        assert_eq!(std::fs::read(other).unwrap(), b"LBUGother");
        refuse_output_in_database_footprint(
            &source,
            &dir.path().join("other.lbug.daily.nwsnap.zst"),
        )
        .unwrap();
    }

    #[test]
    fn marker_filenames_do_not_authorize_recursive_restore_deletion() {
        for marker in ["publication.json", "fake.lbug", "fake.kuzu"] {
            let dir = tempfile::tempdir().unwrap();
            std::fs::write(dir.path().join(marker), b"{}").unwrap();
            std::fs::write(dir.path().join("thesis.txt"), b"irreplaceable").unwrap();
            let error = refuse_replacing_foreign_directory(dir.path()).unwrap_err();
            assert!(error.downcast_ref::<BackupRefusal>().is_some());
            assert_eq!(
                std::fs::read(dir.path().join("thesis.txt")).unwrap(),
                b"irreplaceable"
            );
        }
    }

    #[test]
    fn copied_physical_marker_does_not_authorize_restore_over_a_fake_database() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source");
        std::fs::create_dir(&source).unwrap();
        let db = make_db(&source);
        let target = dir.path().join("foreign");
        std::fs::create_dir(&target).unwrap();
        let fake = target.join("brain.lbug");
        std::fs::write(&fake, b"LBUGnot-a-database").unwrap();
        std::fs::copy(
            nestweaver_store::engine_format::sidecar_path(&db),
            nestweaver_store::engine_format::sidecar_path(&fake),
        )
        .unwrap();
        assert!(!is_recognisable_data_dir(&target).unwrap());
        std::fs::write(target.join("thesis.txt"), b"mine").unwrap();
        assert!(refuse_replacing_foreign_directory(&target).is_err());
        assert_eq!(std::fs::read(target.join("thesis.txt")).unwrap(), b"mine");
    }

    #[test]
    fn unknown_contents_nested_in_a_sidecar_directory_are_preserved() {
        let dir = tempfile::tempdir().unwrap();
        let db = make_db(dir.path());
        assert!(is_recognisable_data_dir(dir.path()).unwrap());
        for suffix in [".publications", ".tantivy"] {
            let sidecar = crate::sidecar_path(&db, suffix);
            std::fs::create_dir_all(&sidecar).unwrap();
            let personal = sidecar.join("thesis.txt");
            std::fs::write(&personal, b"mine").unwrap();
            assert!(refuse_replacing_foreign_directory(dir.path()).is_err());
            assert_eq!(std::fs::read(&personal).unwrap(), b"mine");
            std::fs::remove_dir_all(sidecar).unwrap();
        }
    }

    fn save_config(db: &Path, output: PathBuf) -> BackupConfig {
        BackupConfig {
            db_path: db.to_path_buf(),
            output_path: output,
            include_clones: false,
            instance_id: "test".into(),
            workspace_path: None,
            overwrite: true,
        }
    }

    fn make_db(dir: &Path) -> PathBuf {
        let db = dir.join("brain.lbug");
        drop(nestweaver_store::GraphStore::open(&db).unwrap());
        db
    }

    /// QA (HIGH): `backup save <live db> --force` renamed the archive over
    /// the database. Every spelling of "the database or one of its files"
    /// must be refused, and the database left byte-for-byte alone.
    #[test]
    fn save_refuses_a_destination_in_the_database_footprint() {
        let dir = tempfile::tempdir().unwrap();
        let db = make_db(dir.path());
        let engine_format = crate::sidecar_path(&db, ".engine-format-test");
        std::fs::write(&engine_format, "sidecar").unwrap();
        let tantivy = crate::sidecar_path(&db, ".tantivy-test");
        std::fs::create_dir_all(&tantivy).unwrap();
        let mut destinations = vec![
            db.clone(),
            crate::sidecar_path(&db, ".wal"),
            engine_format.clone(),
            tantivy.join("inside.nwsnap.zst"),
            dir.path().join(".").join("brain.lbug"),
        ];
        #[cfg(unix)]
        {
            let symlink = dir.path().join("link-to-db.nwsnap.zst");
            std::os::unix::fs::symlink(&db, &symlink).unwrap();
            destinations.push(symlink);
            let hardlink = dir.path().join("hard-to-db.nwsnap.zst");
            std::fs::hard_link(&db, &hardlink).unwrap();
            destinations.push(hardlink);
            let symlinked_dir = dir.path().join("alias");
            std::os::unix::fs::symlink(dir.path(), &symlinked_dir).unwrap();
            destinations.push(symlinked_dir.join("brain.lbug"));
        }
        for output in destinations {
            let error = backup_save(&save_config(&db, output.clone()))
                .expect_err(&format!("{} must be refused", output.display()));
            assert!(
                error.to_string().contains("is the database"),
                "{}: {error:#}",
                output.display()
            );
        }
        // Opening the store for the save rewrites header counters, so bytes
        // are not compared: the database must still BE the database (an
        // archive renamed over it would start with the zstd magic instead).
        assert_eq!(
            &std::fs::read(&db).unwrap()[..4],
            b"LBUG",
            "database replaced"
        );
        nestweaver_store::GraphStore::open_read_only(&db).expect("database still opens");
        assert_eq!(std::fs::read(&engine_format).unwrap(), b"sidecar");
        assert!(!crate::sidecar_path(&db, ".wal").exists());

        // Counterweight: an archive named after the database beside it, and
        // one elsewhere, are still allowed.
        for output in [
            crate::sidecar_path(&db, ".2026-10-21.nwsnap.zst"),
            dir.path().join("elsewhere.nwsnap.zst"),
        ] {
            backup_save(&save_config(&db, output.clone())).unwrap();
            assert!(output.exists());
        }
    }

    /// QA (HIGH): `backup restore --force --data-dir <dir>` deleted any
    /// non-empty directory. `--force` must only replace a directory that is
    /// recognisably a NestWeaver data directory.
    #[test]
    fn forced_restore_refuses_a_foreign_directory() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source");
        std::fs::create_dir_all(&source).unwrap();
        let db = make_db(&source);
        let archive = dir.path().join("b.nwsnap.zst");
        let mut config = save_config(&db, archive.clone());
        config.overwrite = false;
        backup_save(&config).unwrap();

        let foreign = dir.path().join("home");
        std::fs::create_dir_all(foreign.join("Documents")).unwrap();
        std::fs::write(foreign.join("notes.txt"), "mine").unwrap();
        let error = backup_restore(&RestoreConfig {
            snapshot_path: archive.clone(),
            data_dir: foreign.clone(),
            replace: true,
        })
        .expect_err("a foreign directory must be refused even with --force");
        assert!(
            error
                .to_string()
                .contains("does not look like a NestWeaver data"),
            "{error:#}"
        );
        assert_eq!(std::fs::read(foreign.join("notes.txt")).unwrap(), b"mine");
        assert!(foreign.join("Documents").is_dir());

        // Counterweight: a directory holding a database is still replaced.
        let data = dir.path().join("data");
        std::fs::create_dir_all(&data).unwrap();
        make_db(&data);
        std::fs::write(data.join("stale.txt"), "old").unwrap();
        let refusal = backup_restore(&RestoreConfig {
            snapshot_path: archive.clone(),
            data_dir: data.clone(),
            replace: true,
        })
        .unwrap_err();
        assert!(refusal.downcast_ref::<BackupRefusal>().is_some());
        assert_eq!(std::fs::read(data.join("stale.txt")).unwrap(), b"old");
        std::fs::remove_file(data.join("stale.txt")).unwrap();
        backup_restore(&RestoreConfig {
            snapshot_path: archive,
            data_dir: data.clone(),
            replace: true,
        })
        .unwrap();
        assert!(data.join("brain.lbug").exists());
    }

    /// QA (LOW): a killed save left `<archive>.partial.<pid>.<n>` and a
    /// killed restore left its staging directory, forever. The next save /
    /// restore removes a dead pid's leftovers and keeps a live pid's.
    #[cfg(unix)]
    #[test]
    fn save_and_restore_sweep_a_dead_runs_leftovers() {
        let dead = {
            let mut child = std::process::Command::new("true").spawn().unwrap();
            let pid = child.id();
            child.wait().unwrap();
            pid
        };
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source");
        std::fs::create_dir_all(&source).unwrap();
        let db = make_db(&source);
        let archive = dir.path().join("b.nwsnap.zst");
        let dead_partial = dir.path().join(format!("b.nwsnap.zst.partial.{dead}.0"));
        let live_partial = dir.path().join("b.nwsnap.zst.partial.1.0");
        std::fs::write(&dead_partial, "x").unwrap();
        std::fs::write(&live_partial, "x").unwrap();
        backup_save(&save_config(&db, archive.clone())).unwrap();
        assert!(!dead_partial.exists(), "dead save's partial is swept");
        assert!(live_partial.exists(), "a live pid's partial is kept");

        let data = dir.path().join("data");
        let dead_staging = dir.path().join(format!(".data.restore-staging.{dead}.abc"));
        let live_staging = dir.path().join(".data.restore-staging.1.abc");
        std::fs::create_dir_all(dead_staging.join("inner")).unwrap();
        std::fs::create_dir_all(&live_staging).unwrap();
        backup_restore(&RestoreConfig {
            snapshot_path: archive,
            data_dir: data.clone(),
            replace: false,
        })
        .unwrap();
        assert!(
            !dead_staging.exists(),
            "dead restore's staging dir is swept"
        );
        assert!(live_staging.exists());
        assert!(data.join("brain.lbug").exists());
    }
}
