//! The parse cache: every indexed file's parse, keyed by content hash.
//!
//! Two files beside the database:
//!
//! * `<db>.parsed_cache.bin`, the base: one MessagePack map, replaced
//!   atomically by a full index ([`ParsedCache::save`]);
//! * `<db>.parsed_cache.log`, an append-only log of entries added since
//!   ([`append_entries`]): incremental and watcher writes and the
//!   whole-graph cross-repo inference add a few parses without rewriting the
//!   base. A full save folds the log into the base and removes it; a log
//!   that outgrows the base is compacted the same way.
//!
//! Loading reads the base, then the log (a torn tail is ignored). A
//! long-lived reader keeps its cache and [`ParsedCache::refresh`]es it,
//! which reads only log records added since, unless the base was replaced.
//!
//! Writers within one process serialize on a lock and merge instead of
//! overwriting: a full save keeps entries another writer added after this
//! cache was loaded.
//!
//! Invariant: only the process holding the database's write lease writes
//! these files (one writer process at a time). The in-process lock is the
//! only coordination, so appends from two processes could interleave; the
//! lease is what rules that out. Debug builds assert that nobody else
//! changed the log since this process last appended to it.

use std::collections::{HashMap, HashSet};
use std::io::{Read, Seek, Write};
use std::path::{Path, PathBuf};

use nestweaver_parser::{AstTypeBinding, RawReference, RawSymbol};
use serde::{Deserialize, Serialize};

/// Bump whenever a parser or query change alters what a file parses to: the
/// cache is keyed by content hash alone, so an unchanged file would otherwise
/// keep its old parse through every non-`--force` index.
///
/// 2 — nw-688: JS/TS test blocks are named `<runner> <title>`, `require()` is
///     the only call-shaped import, and package bindings are recorded.
/// 3 — exact import bindings, Swift receivers, async function values.
const CACHE_VERSION: u32 = 3;

/// A log larger than this (and than the base) is folded into the base.
const LOG_COMPACT_MIN_BYTES: u64 = 64 * 1024 * 1024;

/// Upper bound on one log record, so a corrupt length cannot allocate
/// without bound.
const MAX_LOG_RECORD_BYTES: u32 = 256 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CachedParseResult {
    pub symbols: Vec<RawSymbol>,
    pub references: Vec<RawReference>,
    pub type_bindings: Vec<AstTypeBinding>,
}

#[derive(Debug, Serialize, Deserialize)]
struct ParsedCacheFile {
    version: u32,
    entries: HashMap<String, CachedParseResult>,
}

/// The base file, serialized from borrowed entries (no copy of the map).
#[derive(Serialize)]
struct ParsedCacheFileRef<'a, 'b> {
    version: u32,
    entries: &'b HashMap<&'a str, &'a CachedParseResult>,
}

/// One log record: `u32` little-endian length, then this in MessagePack.
#[derive(Serialize, Deserialize)]
struct LogRecord<'a> {
    version: u32,
    #[serde(borrow)]
    hash: std::borrow::Cow<'a, str>,
    entry: std::borrow::Cow<'a, CachedParseResult>,
}

/// Per log: the length this process last left it at, all complete records.
static VALID_LOG_LEN: std::sync::LazyLock<std::sync::Mutex<HashMap<PathBuf, u64>>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(HashMap::new()));

/// Serializes every write to a cache in this process.
static WRITE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn write_lock() -> std::sync::MutexGuard<'static, ()> {
    WRITE_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// `<db>.parsed_cache.log` for the base at `path`.
pub fn log_path(path: &Path) -> PathBuf {
    path.with_extension("log")
}

/// What identifies one version of the base file.
#[derive(Debug, Clone, PartialEq, Eq)]
struct FileIdentity {
    len: u64,
    modified: Option<std::time::SystemTime>,
    #[cfg(unix)]
    inode: u64,
}

fn identity(path: &Path) -> Option<FileIdentity> {
    let meta = std::fs::metadata(path).ok()?;
    Some(FileIdentity {
        len: meta.len(),
        modified: meta.modified().ok(),
        #[cfg(unix)]
        inode: std::os::unix::fs::MetadataExt::ino(&meta),
    })
}

pub struct ParsedCache {
    entries: HashMap<String, CachedParseResult>,
    /// Keys present when loaded (base and log): a save keeps disk entries
    /// outside this set, which another writer added meanwhile, and drops
    /// ones inside it that this cache evicted.
    loaded_keys: HashSet<String>,
    base: Option<FileIdentity>,
    /// Log bytes already applied.
    log_offset: u64,
}

fn read_base(path: &Path) -> HashMap<String, CachedParseResult> {
    match std::fs::read(path) {
        Ok(data) => match rmp_serde::from_slice::<ParsedCacheFile>(&data) {
            Ok(file) if file.version == CACHE_VERSION => file.entries,
            Ok(_) => {
                tracing::debug!("parsed cache version mismatch, starting fresh");
                HashMap::new()
            }
            Err(_) => {
                tracing::debug!("parsed cache corrupt or unreadable, starting fresh");
                HashMap::new()
            }
        },
        Err(_) => HashMap::new(),
    }
}

/// Apply log records from byte `offset` on; returns the offset after the
/// last complete record. A torn or corrupt tail ends the read.
fn read_log(path: &Path, offset: u64, mut apply: impl FnMut(String, CachedParseResult)) -> u64 {
    let Ok(mut file) = std::fs::File::open(log_path(path)) else {
        return offset;
    };
    if file.seek(std::io::SeekFrom::Start(offset)).is_err() {
        return offset;
    }
    let mut bytes = Vec::new();
    if file.read_to_end(&mut bytes).is_err() {
        return offset;
    }
    let mut at = 0usize;
    while bytes.len() - at >= 4 {
        let len = u32::from_le_bytes(bytes[at..at + 4].try_into().expect("four bytes"));
        if len > MAX_LOG_RECORD_BYTES || bytes.len() - at - 4 < len as usize {
            break;
        }
        let body = &bytes[at + 4..at + 4 + len as usize];
        match rmp_serde::from_slice::<LogRecord<'_>>(body) {
            Ok(record) => {
                if record.version == CACHE_VERSION {
                    apply(record.hash.into_owned(), record.entry.into_owned());
                }
            }
            Err(_) => break,
        }
        at += 4 + len as usize;
    }
    offset + at as u64
}

fn log_len(path: &Path) -> u64 {
    std::fs::metadata(log_path(path))
        .map(|meta| meta.len())
        .unwrap_or(0)
}

impl ParsedCache {
    /// Load a parsed cache from disk (base, then log). Returns an empty
    /// cache on missing/corrupt/version-mismatch files.
    pub fn load(path: &Path) -> Self {
        let base = identity(path);
        let mut entries = read_base(path);
        let log_offset = read_log(path, 0, |hash, entry| {
            entries.insert(hash, entry);
        });
        let loaded_keys = entries.keys().cloned().collect();
        Self {
            entries,
            loaded_keys,
            base,
            log_offset,
        }
    }

    /// A cache that has read nothing yet: its first [`Self::refresh`] loads
    /// the files.
    pub fn empty() -> Self {
        Self {
            entries: HashMap::new(),
            loaded_keys: HashSet::new(),
            base: None,
            log_offset: 0,
        }
    }

    /// Catch up with what other writers added since this cache was loaded:
    /// only new log records, unless the base was replaced (then a reload).
    pub fn refresh(&mut self, path: &Path) {
        if identity(path) != self.base || log_len(path) < self.log_offset {
            *self = Self::load(path);
            return;
        }
        let entries = &mut self.entries;
        let loaded_keys = &mut self.loaded_keys;
        self.log_offset = read_log(path, self.log_offset, |hash, entry| {
            loaded_keys.insert(hash.clone());
            entries.insert(hash, entry);
        });
    }

    /// Look up a cached parse result by content hash.
    pub fn get(&self, content_hash: &str) -> Option<&CachedParseResult> {
        self.entries.get(content_hash)
    }

    /// Insert or update a cached parse result keyed by content hash.
    pub fn insert(&mut self, content_hash: String, result: CachedParseResult) {
        self.entries.insert(content_hash, result);
    }

    /// Persist the cache as the new base (atomic temp file + rename) and
    /// remove the log it supersedes.
    ///
    /// Merge, not overwrite: under the write lock, entries another writer
    /// put on disk after this cache was loaded (a full index of another
    /// repository, a log append) are kept.
    pub fn save(&self, path: &Path) -> Result<(), anyhow::Error> {
        let _lock = write_lock();
        let keep =
            |hash: &str| !self.loaded_keys.contains(hash) && !self.entries.contains_key(hash);
        let mut newer = HashMap::new();
        if identity(path) != self.base || log_len(path) < self.log_offset {
            // Another full save replaced the base: merge all of it.
            let mut disk = Self::load(path);
            disk.entries.retain(|hash, _| keep(hash));
            newer = disk.entries;
        } else {
            // Only the log grew: merge just its new records.
            read_log(path, self.log_offset, |hash, entry| {
                if keep(&hash) {
                    newer.insert(hash, entry);
                }
            });
        }
        let mut entries: HashMap<&str, &CachedParseResult> =
            HashMap::with_capacity(self.entries.len() + newer.len());
        for (hash, entry) in self.entries.iter().chain(newer.iter()) {
            entries.insert(hash.as_str(), entry);
        }
        write_base(path, &entries)?;
        remove_log(path);
        Ok(())
    }

    /// Number of entries in the cache.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the cache is empty.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Evict entries whose content hash is not in `live_hashes`
    /// (i.e. files that were deleted or renamed since the last run).
    pub fn retain_hashes(&mut self, live_hashes: &std::collections::HashSet<String>) {
        self.entries.retain(|hash, _| live_hashes.contains(hash));
    }
}

fn write_base(
    path: &Path,
    entries: &HashMap<&str, &CachedParseResult>,
) -> Result<(), anyhow::Error> {
    let file = ParsedCacheFileRef {
        version: CACHE_VERSION,
        entries,
    };
    let data = rmp_serde::to_vec(&file).map_err(|e| anyhow::anyhow!("serialize: {e}"))?;
    nestweaver_store::durable_sidecar::atomic_replace_file(path, |out| out.write_all(&data))
        .map_err(|e| anyhow::anyhow!("write: {e}"))?;
    Ok(())
}

/// Forget the log length this process recorded, as a new process would
/// not know it (tests that damage the log by hand).
#[cfg(test)]
pub(crate) fn forget_log_length(path: &Path) {
    VALID_LOG_LEN
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .remove(&log_path(path));
}

fn remove_log(path: &Path) {
    VALID_LOG_LEN
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .remove(&log_path(path));
    match std::fs::remove_file(log_path(path)) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => tracing::warn!(%error, "could not remove the parse cache log"),
    }
}

/// Add `entries` to the cache at `path` by appending to its log: no base
/// rewrite. Best effort (a lost entry costs a later re-parse). A log that
/// has outgrown the base is folded into it.
///
/// Safe only under the database write lease: this process must be the one
/// writer process (see the module doc).
pub fn append_entries<'a>(
    path: &Path,
    entries: impl IntoIterator<Item = (&'a str, &'a CachedParseResult)>,
) {
    let mut buffer = Vec::new();
    for (hash, entry) in entries {
        let record = LogRecord {
            version: CACHE_VERSION,
            hash: std::borrow::Cow::Borrowed(hash),
            entry: std::borrow::Cow::Borrowed(entry),
        };
        match rmp_serde::to_vec(&record) {
            Ok(body) if body.len() <= MAX_LOG_RECORD_BYTES as usize => {
                buffer.extend_from_slice(&(body.len() as u32).to_le_bytes());
                buffer.extend_from_slice(&body);
            }
            Ok(_) => {}
            Err(error) => tracing::warn!(%error, "could not serialize a parse cache entry"),
        }
    }
    if buffer.is_empty() {
        return;
    }
    let _lock = write_lock();
    let log = log_path(path);
    // A torn or corrupt tail (a crash mid-append) would hide every record
    // appended after it: cut the log back to its last complete record
    // first. Checked only when the log is not the length this process
    // last left it.
    let current = log_len(path);
    let known = VALID_LOG_LEN
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(&log)
        .copied();
    // One writer process: once this process has appended, only it (or its
    // own saves, which forget the length) changes the log.
    debug_assert!(
        known.is_none_or(|known| known == current),
        "the parse cache log {} changed outside this process ({known:?} -> {current} bytes): \
         only the database write lease holder may write it",
        log.display()
    );
    if current > 0 && known != Some(current) {
        let valid = read_log(path, 0, |_, _| {});
        if valid < current
            && let Err(error) = std::fs::OpenOptions::new()
                .write(true)
                .open(&log)
                .and_then(|file| file.set_len(valid))
        {
            tracing::warn!(%error, "could not cut a torn parse cache log");
            return;
        }
    }
    let written = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log)
        .and_then(|mut file| file.write_all(&buffer));
    if let Err(error) = written {
        tracing::warn!(%error, path = %log.display(), "could not append to the parse cache log");
        VALID_LOG_LEN
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&log);
        return;
    }
    VALID_LOG_LEN
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(log.clone(), log_len(path));
    let base_len = identity(path).map(|id| id.len).unwrap_or(0);
    let log_len = log_len(path);
    if log_len > LOG_COMPACT_MIN_BYTES && log_len > base_len {
        let cache = ParsedCache::load(path);
        let entries: HashMap<&str, &CachedParseResult> = cache
            .entries
            .iter()
            .map(|(hash, entry)| (hash.as_str(), entry))
            .collect();
        match write_base(path, &entries) {
            Ok(()) => remove_log(path),
            Err(error) => tracing::warn!(%error, "could not compact the parse cache log"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nestweaver_parser::{AstTypeBinding, RawReference, RawSymbol};
    use nestweaver_schema::{SymbolKind, Visibility};

    fn sample_result() -> CachedParseResult {
        CachedParseResult {
            symbols: vec![RawSymbol {
                name: "hello".into(),
                kind: SymbolKind::Function,
                start_line: 1,
                end_line: 3,
                signature: "fn hello()".into(),
                content_hash: "abc123".into(),
                is_entry_point: false,
                entry_point_kind: None,
                visibility: Visibility::Public,
                type_info: None,
                parent_name: None,
                scope_chain: None,
            }],
            references: vec![RawReference {
                name: "world".into(),
                kind: nestweaver_parser::ReferenceKind::Call,
                start_line: 2,
                context: String::new(),
                receiver: None,
            }],
            type_bindings: vec![AstTypeBinding {
                var_name: "x".into(),
                type_name: "i32".into(),
                line: 1,
                kind: nestweaver_parser::AstBindingKind::Annotation,
            }],
        }
    }

    #[test]
    fn round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("test.parsed_cache.bin");

        let mut cache = ParsedCache::load(&path);
        assert!(cache.is_empty());

        cache.insert("hash1".into(), sample_result());
        cache.insert("hash2".into(), sample_result());
        assert_eq!(cache.len(), 2);

        cache.save(&path).unwrap();

        let loaded = ParsedCache::load(&path);
        assert_eq!(loaded.len(), 2);
        assert!(loaded.get("hash1").is_some());
        assert!(loaded.get("hash2").is_some());
        assert!(loaded.get("nonexistent").is_none());

        let result = loaded.get("hash1").unwrap();
        assert_eq!(result.symbols.len(), 1);
        assert_eq!(result.symbols[0].name, "hello");
        assert_eq!(result.references.len(), 1);
        assert_eq!(result.references[0].name, "world");
        assert_eq!(result.type_bindings.len(), 1);
        assert_eq!(result.type_bindings[0].var_name, "x");
    }

    #[test]
    fn old_import_receiver_parse_cache_is_rejected_and_reparsed() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("test.parsed_cache.bin");
        let source = "const work = async function() { helper(); };\n";
        let hash = "unchanged-source";
        let mut stale = sample_result();
        stale.symbols[0].name = "old_proxy".into();
        let file = ParsedCacheFile {
            version: 2,
            entries: HashMap::from([(hash.to_string(), stale)]),
        };
        std::fs::write(&path, rmp_serde::to_vec(&file).unwrap()).unwrap();
        let mut cache = ParsedCache::load(&path);
        let parsed = match cache.get(hash) {
            Some(hit) => hit.clone(),
            None => {
                let fresh =
                    nestweaver_parser::parse_source(std::path::Path::new("main.js"), source)
                        .unwrap();
                CachedParseResult {
                    symbols: fresh.symbols,
                    references: fresh.references,
                    type_bindings: fresh.type_bindings,
                }
            }
        };
        assert!(
            !parsed
                .symbols
                .iter()
                .any(|symbol| symbol.name == "old_proxy")
        );
        assert_eq!(
            parsed
                .symbols
                .iter()
                .filter(|symbol| symbol.name == "work")
                .count(),
            1
        );
        cache.insert(hash.into(), parsed);
        cache.save(&path).unwrap();
        assert!(
            ParsedCache::load(&path)
                .get(hash)
                .unwrap()
                .symbols
                .iter()
                .any(|symbol| symbol.name == "work" && symbol.kind == SymbolKind::Function)
        );
    }

    #[test]
    fn version_mismatch_returns_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("test.parsed_cache.bin");

        // Write a cache with a different version
        let file = ParsedCacheFile {
            version: 999,
            entries: HashMap::new(),
        };
        let data = rmp_serde::to_vec(&file).unwrap();
        std::fs::write(&path, data).unwrap();

        let cache = ParsedCache::load(&path);
        assert!(cache.is_empty());
    }

    /// nw-688: a parse cached before the test-title change (version 1, where
    /// `describe('getTier')` parsed to a Function `getTier`) must not be
    /// reused by an incremental index of the unchanged file.
    #[test]
    fn a_pre_nw_688_parse_is_not_reused() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("test.parsed_cache.bin");
        let mut stale = sample_result();
        stale.symbols[0].name = "getTier".into();
        let file = ParsedCacheFile {
            version: 1,
            entries: HashMap::from([("hash1".to_string(), stale)]),
        };
        std::fs::write(&path, rmp_serde::to_vec(&file).unwrap()).unwrap();

        let cache = ParsedCache::load(&path);
        assert!(cache.get("hash1").is_none(), "a version-1 parse was reused");
    }

    #[test]
    fn corrupt_file_returns_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("test.parsed_cache.bin");

        std::fs::write(&path, b"not valid msgpack").unwrap();

        let cache = ParsedCache::load(&path);
        assert!(cache.is_empty());
    }

    #[test]
    fn missing_file_returns_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("nonexistent.bin");

        let cache = ParsedCache::load(&path);
        assert!(cache.is_empty());
    }

    /// Two writers that loaded the same cache must not lose each other's
    /// entries: a full save merges what another writer put on disk since
    /// (its full save, or a log append), in either order.
    #[test]
    fn interleaved_writers_keep_each_others_entries() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("g.parsed_cache.bin");
        let mut seed = ParsedCache::load(&path);
        seed.insert("old".into(), sample_result());
        seed.save(&path).unwrap();

        // Writer A (a full index) and writer B (another full index) both
        // load, then save in turn.
        let mut a = ParsedCache::load(&path);
        let mut b = ParsedCache::load(&path);
        b.insert("from-b".into(), sample_result());
        b.save(&path).unwrap();
        a.insert("from-a".into(), sample_result());
        a.save(&path).unwrap();
        let merged = ParsedCache::load(&path);
        for hash in ["old", "from-a", "from-b"] {
            assert!(merged.get(hash).is_some(), "{hash} lost");
        }

        // A log append (the whole-graph pass, a watcher batch) between
        // another writer's load and its full save survives the save.
        let mut c = ParsedCache::load(&path);
        let appended = sample_result();
        append_entries(&path, [("appended", &appended)]);
        c.insert("from-c".into(), sample_result());
        c.save(&path).unwrap();
        let merged = ParsedCache::load(&path);
        for hash in ["old", "from-a", "from-b", "appended", "from-c"] {
            assert!(merged.get(hash).is_some(), "{hash} lost");
        }
        assert!(!log_path(&path).exists(), "a full save folds in the log");
    }

    /// An entry this writer evicted is not resurrected by the merge.
    #[test]
    fn a_save_keeps_its_own_evictions() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("g.parsed_cache.bin");
        let mut seed = ParsedCache::load(&path);
        seed.insert("dead".into(), sample_result());
        seed.insert("live".into(), sample_result());
        seed.save(&path).unwrap();
        let mut cache = ParsedCache::load(&path);
        append_entries(&path, [("other", &sample_result())]);
        // Another writer replaces the base meanwhile, still holding "dead".
        let mut rival = ParsedCache::load(&path);
        rival.insert("rival".into(), sample_result());
        rival.save(&path).unwrap();
        cache.retain_hashes(&HashSet::from(["live".to_string()]));
        cache.save(&path).unwrap();
        let saved = ParsedCache::load(&path);
        assert!(saved.get("dead").is_none(), "an eviction was undone");
        for hash in ["live", "other", "rival"] {
            assert!(saved.get(hash).is_some(), "{hash} lost");
        }
    }

    /// A long-lived cache reads only what was appended since; a torn tail is
    /// cut before the next append, so later records stay readable.
    #[test]
    fn refresh_reads_appends_and_a_torn_tail_is_cut() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("g.parsed_cache.bin");
        let mut seed = ParsedCache::load(&path);
        seed.insert("base".into(), sample_result());
        seed.save(&path).unwrap();
        let mut cache = ParsedCache::load(&path);
        append_entries(&path, [("one", &sample_result())]);
        cache.refresh(&path);
        assert!(cache.get("one").is_some());

        let mut log = std::fs::OpenOptions::new()
            .append(true)
            .open(log_path(&path))
            .unwrap();
        log.write_all(&[7, 0, 0, 0, 1, 2]).unwrap();
        drop(log);
        // A torn tail is a crash: the next append is a new process.
        forget_log_length(&path);
        append_entries(&path, [("two", &sample_result())]);
        let loaded = ParsedCache::load(&path);
        for hash in ["base", "one", "two"] {
            assert!(loaded.get(hash).is_some(), "{hash} unreadable");
        }
        cache.refresh(&path);
        assert!(cache.get("two").is_some());
    }

    /// Debug builds catch a second writer: the log changed behind this
    /// process's last append.
    #[cfg(debug_assertions)]
    #[test]
    #[should_panic(expected = "changed outside this process")]
    fn a_second_log_writer_trips_the_debug_assertion() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("g.parsed_cache.bin");
        append_entries(&path, [("one", &sample_result())]);
        let mut log = std::fs::OpenOptions::new()
            .append(true)
            .open(log_path(&path))
            .unwrap();
        log.write_all(&[0, 0, 0, 0]).unwrap();
        drop(log);
        append_entries(&path, [("two", &sample_result())]);
    }
}
