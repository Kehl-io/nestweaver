//! F16: a generation-keyed response cache for deterministic read tools.
//!
//! # Why correctness rests on the KEY, not a daemon
//!
//! The original F16 design's correctness depended on a background watcher
//! sweep to evict stale entries AND on an in-memory `graph_generation` that
//! reset to 0 every time a process opened the store. Both are fragile: a
//! short-lived MCP process has no sweep, and a reset counter makes every
//! entry look current.
//!
//! This implementation moves correctness entirely onto the cache *key check*:
//!
//! - Every entry records the [`graph_generation`](crate::db::GraphStore::graph_generation)
//!   that was current when it was written (the *persisted* value — see P0.2).
//! - On read, an entry is a HIT only if `entry.generation == store.graph_generation()`.
//!   Because the generation is persisted to `<db>.generation` and bumped at the
//!   end of every index/reindex, a fresh process that opens the store after a
//!   reindex observes the new generation and treats every older entry as a
//!   MISS — with no running daemon.
//! - As a second guard, each entry records a `scope_digest` — a hash over the
//!   content-hashes in `<db>.filemeta.json`. If the underlying files changed
//!   (and were re-indexed) the digest moves, so even a generation collision
//!   would still miss.
//!
//! A background sweep MAY still run for opportunistic eviction (reclaiming disk
//! space), but it is never load-bearing for correctness.
//!
//! # The third guard: response SHAPE (H1)
//!
//! `generation` and `scope_digest` both describe the GRAPH. Neither describes
//! the BINARY. That left a real hole: upgrade to a build that adds a field to a
//! cached tool's response, leave the graph untouched, repeat a query from the
//! last 24h, and the pre-upgrade entry still satisfies every check — so the OLD
//! response shape is served, missing the new field, for up to a full TTL.
//!
//! So every entry also records the `shape_version` of the binary that wrote
//! it, supplied at [`ResponseCache::open`]. A HIT requires
//! equality, and entries with a foreign shape version are dropped on open.
//! The check lives HERE rather than in each caller's key derivation so that no
//! future call site can forget it.
//!
//! The value is not hand-maintained: `nestweaver-mcp`'s `build.rs` derives
//! `RESPONSE_SHAPE_VERSION` as a content digest of the workspace sources, so a
//! response-shape change invalidates the cache by construction. This module
//! deliberately does NOT define that constant — it must come from the crate
//! that produces the responses, and passing `0` disables the guard.
//!
//! # Honest framing
//!
//! The cache returns results consistent with the LAST INDEX. If files changed
//! on disk but were not re-indexed, the cache yields the same (stale) answer
//! the live graph would yield — the staleness of the cache is exactly the
//! staleness of the graph, which is the correct semantic. The hit-rate is
//! unproven and should be measured in practice.

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use rmp_serde;
use serde::{Deserialize, Serialize};

/// Entries older than this never hit (seconds). 24h.
pub const TTL_SECS: f64 = 24.0 * 60.0 * 60.0;

/// Default LRU size cap when no `[cache] max_size_mb` is configured.
pub const DEFAULT_MAX_SIZE_MB: u64 = 256;

/// ZSTD compression level. Level 3 is the default speed/ratio trade-off.
const ZSTD_LEVEL: i32 = 3;

/// Magic bytes that identify the binary cache format (MessagePack + ZSTD).
const CACHE_MAGIC: &[u8; 4] = b"NWRC";

/// Binary format version byte.
const CACHE_VERSION: u8 = 1;

/// One cached tool response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CacheEntry {
    /// Stable hash of `(tool_name, normalized_args)`.
    pub key_hash: u64,
    /// The tool that produced this response (for diagnostics + per-tool stats).
    pub tool: String,
    /// ZSTD-compressed response bytes (the serialized JSON result).
    pub response: Vec<u8>,
    /// Unix epoch seconds when the entry was written.
    pub created_at: f64,
    /// Persisted `graph_generation` at write time. HIT requires equality.
    pub generation: u64,
    /// Digest over filemeta content-hashes at write time. HIT requires equality.
    pub scope_digest: u64,
    /// Last time this entry was read (for LRU). Defaults to `created_at`.
    #[serde(default)]
    pub last_access: f64,
    /// Response-shape version of the binary that wrote this entry. HIT requires
    /// equality (H1).
    ///
    /// Entries from a sidecar written before this field existed decode as `0`
    /// on BOTH paths, and are therefore dropped by [`ResponseCache::open`]: a
    /// real shape version is never `0` (`nestweaver-mcp`'s `build.rs` sets the
    /// low bit to guarantee it). MessagePack encodes structs positionally, so
    /// the old entry is a shorter array — but `#[serde(default)]` makes the
    /// derived `visit_seq` substitute the default for the missing trailing
    /// element rather than erroring, exactly as it already does for
    /// `last_access` above. Both the binary and legacy-JSON paths are covered
    /// by tests; do not assume the shorter array fails to decode.
    #[serde(default)]
    pub shape_version: u64,
}

impl CacheEntry {
    /// Approximate on-disk/in-memory size of this entry in bytes.
    fn size_bytes(&self) -> u64 {
        // Response payload dominates; add a small fixed overhead for metadata.
        self.response.len() as u64 + self.tool.len() as u64 + 64
    }
}

/// On-disk cache document (the `<db>.cache` sidecar).
#[derive(Debug, Default, Serialize, Deserialize)]
struct CacheDoc {
    entries: Vec<CacheEntry>,
}

/// The response cache backed by the `<db>.cache` sidecar.
///
/// This is a simple load-modify-save store: it is opened per operation,
/// mirroring the rest of the codebase's sidecar pattern. Concurrency is
/// last-writer-wins, which is acceptable because the cache is advisory — a
/// lost write merely costs a recompute, never correctness.
pub struct ResponseCache {
    path: PathBuf,
    entries: HashMap<u64, CacheEntry>,
    max_size_bytes: u64,
    /// Response-shape identity of THIS binary. Entries written under any other
    /// value are dropped on open and can never HIT. See the module docs (H1).
    shape_version: u64,
}

/// Create the staging file `flush` writes through, in the target's own
/// directory (a cross-filesystem `rename` would fail with `EXDEV`).
///
/// Extracted so the property that actually matters is directly observable: the
/// staging path must be UNIQUE PER CALL. The defect this replaced was a
/// constant — `<db>.cache.tmp` — and a constant is what makes two writers
/// collide. Testing the collision itself is timing-dependent and unreliable;
/// testing that the generator does not return the same path twice is neither.
fn stage_temp_file(target: &Path) -> std::io::Result<tempfile::NamedTempFile> {
    let parent = target.parent().unwrap_or(Path::new("."));
    tempfile::NamedTempFile::new_in(parent)
}

impl ResponseCache {
    /// Path to the `<db>.cache` sidecar for a database path.
    pub fn sidecar_path(db_path: &Path) -> PathBuf {
        let mut s = db_path.as_os_str().to_owned();
        s.push(".cache");
        PathBuf::from(s)
    }

    /// Open (or create empty) the cache for `db_path` with a size cap in MiB.
    /// A corrupt or absent sidecar yields an empty cache (never an error path).
    ///
    /// `shape_version` identifies the response shapes THIS binary produces —
    /// pass `nestweaver_mcp::tools::RESPONSE_SHAPE_VERSION` (or, in tests, any
    /// stable non-zero value). Entries written under a different shape version
    /// are discarded here and can never be served (H1). Passing `0` means "this
    /// caller makes no shape claim"; it still partitions against every real
    /// (non-zero) shape version, but two `0` callers share a namespace.
    ///
    /// Format detection:
    /// - Starts with `NWRC` + version byte → binary (MessagePack + ZSTD).
    /// - Otherwise → JSON legacy fallback.
    pub fn open(db_path: &Path, max_size_mb: u64, shape_version: u64) -> Self {
        let path = Self::sidecar_path(db_path);
        let entries = std::fs::read(&path)
            .ok()
            .and_then(|bytes| decode_cache_bytes(&bytes).ok())
            .map(|doc| {
                doc.entries
                    .into_iter()
                    // Drop foreign-shape entries at load: they can never HIT,
                    // and keeping them would let them occupy the size cap.
                    .filter(|e| e.shape_version == shape_version)
                    .map(|e| (e.key_hash, e))
                    .collect::<HashMap<_, _>>()
            })
            .unwrap_or_default();
        Self {
            path,
            entries,
            max_size_bytes: max_size_mb.saturating_mul(1024 * 1024),
            shape_version,
        }
    }

    /// Compute the stable cache key for `(tool, args)`.
    ///
    /// `args` is normalized first: object keys are sorted recursively and the
    /// non-semantic keys `debug`, `json`, `db`, `config`, `cache`, and
    /// `no_cache` are dropped, so two calls that differ only in those flags
    /// share a key.
    pub fn key(tool: &str, args: &serde_json::Value) -> u64 {
        let normalized = normalize_args(args);
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        tool.hash(&mut hasher);
        // serde_json::Value of a normalized (sorted-keys) tree serializes
        // deterministically, giving a stable string to hash.
        normalized.to_string().hash(&mut hasher);
        hasher.finish()
    }

    /// Look up a cached response.
    ///
    /// Returns `Some(decompressed_bytes)` only on a HIT: the entry must exist,
    /// its `generation` must equal `generation`, its `scope_digest` must equal
    /// `scope_digest`, its `shape_version` must equal this cache's, and it must
    /// not be older than [`TTL_SECS`]. Any mismatch is a MISS. A HIT updates the
    /// entry's `last_access` (LRU) in memory; the caller may persist it via
    /// [`ResponseCache::save`].
    pub fn get(&mut self, key: u64, generation: u64, scope_digest: u64) -> Option<Vec<u8>> {
        let now = now_secs();
        let shape_version = self.shape_version;
        let entry = self.entries.get_mut(&key)?;
        // Defense in depth: `open` already drops foreign-shape entries, but an
        // in-memory cache that outlives a shape change must still refuse them.
        if entry.shape_version != shape_version {
            return None;
        }
        if entry.generation != generation {
            return None;
        }
        if entry.scope_digest != scope_digest {
            return None;
        }
        if now - entry.created_at > TTL_SECS {
            return None;
        }
        entry.last_access = now;
        crate::zstd::decode_all(entry.response.as_slice()).ok()
    }

    /// Insert (or replace) a response for `key`. `response` is the raw
    /// (uncompressed) JSON bytes; it is ZSTD-compressed at level 3 before
    /// storage. After insertion the cache is trimmed to its size cap via LRU.
    pub fn insert(
        &mut self,
        key: u64,
        tool: &str,
        response: &[u8],
        generation: u64,
        scope_digest: u64,
    ) {
        let compressed = match crate::zstd::encode_all(response, ZSTD_LEVEL) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!("response cache: zstd compress failed: {e}");
                return;
            }
        };
        let now = now_secs();
        self.entries.insert(
            key,
            CacheEntry {
                key_hash: key,
                tool: tool.to_string(),
                response: compressed,
                created_at: now,
                generation,
                scope_digest,
                last_access: now,
                shape_version: self.shape_version,
            },
        );
        self.evict_to_cap();
    }

    /// Opportunistic eviction: drop expired entries, then evict
    /// least-recently-used entries until total size is within the cap. This is
    /// the only "sweep"; it is for space reclamation, never correctness.
    pub fn evict_to_cap(&mut self) {
        let now = now_secs();
        self.entries.retain(|_, e| now - e.created_at <= TTL_SECS);

        let mut total: u64 = self.entries.values().map(|e| e.size_bytes()).sum();
        if total <= self.max_size_bytes {
            return;
        }
        // Sort keys by last_access ascending (oldest first) and drop until
        // under the cap.
        let mut by_lru: Vec<(u64, f64, u64)> = self
            .entries
            .values()
            .map(|e| (e.key_hash, e.last_access, e.size_bytes()))
            .collect();
        by_lru.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
        for (key, _, size) in by_lru {
            if total <= self.max_size_bytes {
                break;
            }
            self.entries.remove(&key);
            total = total.saturating_sub(size);
        }
    }

    /// Persist the cache to its sidecar using binary format (MessagePack + ZSTD).
    /// Replaces the sidecar through a UNIQUE temp file, then renames.
    ///
    /// It previously staged through a FIXED shared name, `<db>.cache.tmp`. The
    /// rename is atomic; writing into a path every other writer also uses is
    /// not — two flushers interleave in that one file and whichever renames
    /// last publishes the blend. This is not a cross-process-only hazard:
    /// `RESPONSE_CACHE` in the MCP crate is a `thread_local`, so every daemon
    /// worker thread holds its own full copy of the map and flushes the WHOLE
    /// file. The race is inside a single daemon.
    ///
    /// Deliberately NOT `durable_sidecar::atomic_replace_file`, which every
    /// other sidecar uses: that fsyncs the temp file and the parent directory,
    /// and this is a CACHE. `open` already treats an unreadable file as empty,
    /// so a flush lost to power failure costs a recomputation and nothing
    /// else. What this needs is isolation from concurrent writers, not
    /// durability — and measured on this path the fsyncs cost ~40x the write
    /// itself, on a flush that runs every 50 cache misses.
    pub fn flush(&self) {
        match self.encode_binary() {
            Ok(bytes) => {
                let mut tmp = match stage_temp_file(&self.path) {
                    Ok(tmp) => tmp,
                    Err(e) => {
                        tracing::warn!("response cache: failed to create tmp sidecar: {e}");
                        return;
                    }
                };
                if let Err(e) = std::io::Write::write_all(&mut tmp, &bytes) {
                    tracing::warn!("response cache: failed to write tmp sidecar: {e}");
                    return;
                }
                if let Err(e) = tmp.persist(&self.path) {
                    tracing::warn!("response cache: failed to rename sidecar: {e}");
                }
            }
            Err(e) => tracing::warn!("response cache: binary encode failed: {e}"),
        }
    }

    /// Persist the cache to its sidecar. Calls [`flush`](Self::flush) internally,
    /// so existing callers get the new binary format automatically.
    pub fn save(&self) {
        self.flush();
    }

    /// Encode the cache as binary: MessagePack → ZSTD → magic header.
    fn encode_binary(&self) -> Result<Vec<u8>, std::io::Error> {
        let doc = CacheDoc {
            entries: self.entries.values().cloned().collect(),
        };
        let msgpack = rmp_serde::to_vec(&doc)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        let compressed = crate::zstd::encode_all(msgpack.as_slice(), ZSTD_LEVEL)?;
        let mut out = Vec::with_capacity(CACHE_MAGIC.len() + 1 + compressed.len());
        out.extend_from_slice(CACHE_MAGIC);
        out.push(CACHE_VERSION);
        out.extend_from_slice(&compressed);
        Ok(out)
    }

    /// Number of entries currently held.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// True when the cache holds no entries.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Total approximate size of all entries in bytes.
    pub fn size_bytes(&self) -> u64 {
        self.entries.values().map(|e| e.size_bytes()).sum()
    }
}

/// Recursively normalize a JSON args value: sort object keys and drop the
/// non-semantic flags that must not affect the cache key.
fn normalize_args(value: &serde_json::Value) -> serde_json::Value {
    const DROP_KEYS: &[&str] = &["debug", "json", "db", "config", "cache", "no_cache"];
    match value {
        serde_json::Value::Object(map) => {
            // BTreeMap gives deterministic key ordering on serialization.
            let mut sorted = serde_json::Map::new();
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            for k in keys {
                if DROP_KEYS.contains(&k.as_str()) {
                    continue;
                }
                if let Some(v) = map.get(k) {
                    sorted.insert(k.clone(), normalize_args(v));
                }
            }
            serde_json::Value::Object(sorted)
        }
        serde_json::Value::Array(items) => {
            serde_json::Value::Array(items.iter().map(normalize_args).collect())
        }
        other => other.clone(),
    }
}

/// Decode raw sidecar bytes into a [`CacheDoc`], detecting format automatically.
///
/// - Starts with `NWRC` + version byte → decompress ZSTD, deserialize MessagePack.
/// - Otherwise → try JSON (legacy fallback).
/// - Any error returns `Err` so the caller can fall back to an empty cache.
fn decode_cache_bytes(bytes: &[u8]) -> Result<CacheDoc, Box<dyn std::error::Error>> {
    if bytes.starts_with(CACHE_MAGIC) {
        let version = *bytes
            .get(CACHE_MAGIC.len())
            .ok_or("truncated binary cache header")?;
        if version != CACHE_VERSION {
            return Err(
                format!("unsupported cache version {version} (expected {CACHE_VERSION})").into(),
            );
        }
        let payload = &bytes[CACHE_MAGIC.len() + 1..];
        let decompressed = crate::zstd::decode_all(payload)?;
        let doc = rmp_serde::from_slice::<CacheDoc>(&decompressed)?;
        Ok(doc)
    } else {
        let doc = serde_json::from_slice::<CacheDoc>(bytes)?;
        Ok(doc)
    }
}

/// Digest over the content-hashes in a filemeta map. Used as the
/// `scope_digest`: the whole-DB digest (simpler than per-query scope and still
/// correct — a wider scope only causes more conservative misses, never an
/// incorrect hit). `filemeta` maps a key (file path) to its content hash.
pub fn scope_digest_from_hashes<'a, I>(content_hashes: I) -> u64
where
    I: IntoIterator<Item = (&'a str, &'a str)>,
{
    // Order-independent: XOR per-(path,hash) hashes so the digest does not
    // depend on iteration order of the underlying map.
    let mut acc: u64 = 0;
    for (path, hash) in content_hashes {
        let mut h = std::collections::hash_map::DefaultHasher::new();
        path.hash(&mut h);
        hash.hash(&mut h);
        acc ^= h.finish();
    }
    acc
}

fn now_secs() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A stable, non-zero stand-in for a real binary's response-shape version.
    const TEST_SHAPE: u64 = 0xA11CE;

    /// The staging path must be UNIQUE PER CALL.
    ///
    /// `flush` used to stage through the constant `<db>.cache.tmp`, and a
    /// constant is precisely what lets two writers collide — they open the same
    /// file, interleave, and whichever renames last publishes the blend.
    /// `RESPONSE_CACHE` in the MCP crate is a `thread_local`, so every daemon
    /// worker thread flushes its own full copy of the map: the collision lives
    /// inside one daemon, not only across processes.
    ///
    /// Three earlier attempts tested the CONSEQUENCE — race many threads, assert
    /// the file is not torn — and every one passed against the buggy code,
    /// because whether two writes actually interleave is timing. The defect is
    /// not the corruption; the defect is the constant. That is deterministic,
    /// single-threaded, and observable here.
    #[test]
    fn the_staging_path_is_unique_per_call() {
        let dir = tempfile::tempdir().unwrap();
        let target = ResponseCache::sidecar_path(&dir.path().join("brain.lbug"));

        let first = stage_temp_file(&target).unwrap();
        let second = stage_temp_file(&target).unwrap();
        assert_ne!(
            first.path(),
            second.path(),
            "two writers would stage through the same file and interleave"
        );

        // Same directory as the target: a cross-filesystem rename fails EXDEV,
        // so a temp in the system temp dir would break `persist` on any setup
        // where the database lives on another mount.
        for staged in [&first, &second] {
            assert_eq!(
                staged.path().parent(),
                target.parent(),
                "staging must happen beside the target so the rename stays on one filesystem"
            );
        }
    }

    /// The reader's corruption tolerance is LOAD-BEARING.
    ///
    /// `flush` deliberately does not fsync: this is a cache, and the worst a
    /// lost write can cost is a recomputation. That trade is only sound because
    /// a torn or zero-filled file reads back as empty rather than as data. If
    /// that ever stops being true, the no-fsync decision silently becomes a
    /// correctness bug — so it is pinned here rather than left as a comment.
    #[test]
    fn a_damaged_sidecar_reads_back_as_empty_rather_than_as_data() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("brain.lbug");
        // `open` DERIVES the sidecar from the db path; it does not use the path
        // it is handed as the file. Reading the wrong file is what made three
        // earlier attempts at this test pass against the buggy implementation.
        let sidecar = ResponseCache::sidecar_path(&db_path);

        let mut cache = ResponseCache::open(&db_path, DEFAULT_MAX_SIZE_MB, TEST_SHAPE);
        let blob = serde_json::to_vec(&json!({ "blob": "payload" })).unwrap();
        for i in 0..8 {
            cache.insert(i, "tool", &blob, 1, 0);
        }
        cache.flush();
        let healthy = ResponseCache::open(&db_path, DEFAULT_MAX_SIZE_MB, TEST_SHAPE).len();
        assert!(healthy > 0, "precondition: a healthy sidecar round-trips");

        let good = std::fs::read(&sidecar).unwrap();
        // The two shapes a crash after a non-fsynced rename actually produces:
        // a truncated file, and a correctly-sized file of zeros (ext4 delayed
        // allocation lands the metadata before the data blocks).
        for (label, damaged) in [
            ("truncated", good[..good.len() / 2].to_vec()),
            ("zero-filled", vec![0u8; good.len()]),
            ("empty", Vec::new()),
        ] {
            std::fs::write(&sidecar, &damaged).unwrap();
            let reopened = ResponseCache::open(&db_path, DEFAULT_MAX_SIZE_MB, TEST_SHAPE);
            assert_eq!(
                reopened.len(),
                0,
                "a {label} sidecar must read back as empty, never as data"
            );
        }
    }

    #[test]
    fn key_drops_non_semantic_flags_and_sorts() {
        let a = json!({ "seeds": ["x"], "debug": true, "json": true, "db": "/p.lbug" });
        let b = json!({ "seeds": ["x"] });
        // Differing only in dropped flags → same key.
        assert_eq!(
            ResponseCache::key("brain_context", &a),
            ResponseCache::key("brain_context", &b)
        );

        // Key order in the object must not matter.
        let c = json!({ "b": 1, "a": 2 });
        let d = json!({ "a": 2, "b": 1 });
        assert_eq!(ResponseCache::key("t", &c), ResponseCache::key("t", &d));

        // Different tool → different key.
        assert_ne!(
            ResponseCache::key("brain_context", &b),
            ResponseCache::key("brain_search", &b)
        );
    }

    #[test]
    fn hit_only_on_generation_and_scope_match() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("t.lbug");
        let mut cache = ResponseCache::open(&db_path, DEFAULT_MAX_SIZE_MB, TEST_SHAPE);

        let key = ResponseCache::key("hub_nodes", &json!({"limit": 10}));
        let payload = br#"{"result":"ok"}"#;
        cache.insert(key, "hub_nodes", payload, 5, 99);

        // Same generation + scope → HIT, byte-identical.
        assert_eq!(cache.get(key, 5, 99).as_deref(), Some(&payload[..]));

        // Generation mismatch → MISS (the load-bearing check).
        assert_eq!(cache.get(key, 6, 99), None);

        // Scope mismatch → MISS.
        assert_eq!(cache.get(key, 5, 100), None);
    }

    #[test]
    fn ttl_expired_entries_miss() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("t.lbug");
        let mut cache = ResponseCache::open(&db_path, DEFAULT_MAX_SIZE_MB, TEST_SHAPE);
        let key = ResponseCache::key("clusters", &json!({}));
        cache.insert(key, "clusters", b"payload", 1, 1);
        // Backdate created_at beyond the TTL.
        if let Some(e) = cache.entries.get_mut(&key) {
            e.created_at = now_secs() - TTL_SECS - 1.0;
        }
        assert_eq!(cache.get(key, 1, 1), None);
    }

    #[test]
    fn persists_across_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("t.lbug");
        let key = ResponseCache::key("clusters", &json!({}));
        {
            let mut cache = ResponseCache::open(&db_path, DEFAULT_MAX_SIZE_MB, TEST_SHAPE);
            cache.insert(key, "clusters", b"hello world", 3, 7);
            cache.save();
        }
        let mut reopened = ResponseCache::open(&db_path, DEFAULT_MAX_SIZE_MB, TEST_SHAPE);
        assert_eq!(
            reopened.get(key, 3, 7).as_deref(),
            Some(&b"hello world"[..])
        );
    }

    #[test]
    fn lru_eviction_respects_cap() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("t.lbug");
        // Tiny cap (0 MiB rounds to 0 bytes) forces eviction of all but the
        // freshest; verify the cache trims rather than growing unbounded.
        let mut cache = ResponseCache::open(&db_path, 0, TEST_SHAPE);
        for i in 0..10u64 {
            let key = ResponseCache::key("t", &json!({ "i": i }));
            cache.insert(key, "t", b"some payload bytes", 1, 1);
        }
        // With a 0-byte cap everything over the cap is evicted; at most a
        // handful survive (the most recently inserted). The invariant we care
        // about is that it does not grow without bound.
        assert!(cache.len() <= 1);
    }

    #[test]
    fn flush_binary_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("t.lbug");
        let key = ResponseCache::key("clusters", &json!({}));
        {
            let mut cache = ResponseCache::open(&db_path, DEFAULT_MAX_SIZE_MB, TEST_SHAPE);
            cache.insert(key, "clusters", b"binary round trip", 7, 42);
            cache.flush();
        }
        // Verify the sidecar starts with magic bytes.
        let raw = std::fs::read(ResponseCache::sidecar_path(&db_path)).unwrap();
        assert!(
            raw.starts_with(b"NWRC"),
            "sidecar should start with NWRC magic"
        );

        let mut reopened = ResponseCache::open(&db_path, DEFAULT_MAX_SIZE_MB, TEST_SHAPE);
        assert_eq!(
            reopened.get(key, 7, 42).as_deref(),
            Some(&b"binary round trip"[..])
        );
    }

    #[test]
    fn json_migration_fallback() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("t.lbug");
        let key = ResponseCache::key("clusters", &json!({}));

        // Write a JSON sidecar directly (legacy format).
        {
            let mut cache = ResponseCache::open(&db_path, DEFAULT_MAX_SIZE_MB, TEST_SHAPE);
            cache.insert(key, "clusters", b"legacy json payload", 3, 9);
            // Bypass flush; serialize as plain JSON.
            let doc = CacheDoc {
                entries: cache.entries.values().cloned().collect(),
            };
            let json_bytes = serde_json::to_vec(&doc).unwrap();
            std::fs::write(ResponseCache::sidecar_path(&db_path), json_bytes).unwrap();
        }

        // open() should fall back to JSON and serve the entry.
        let mut reopened = ResponseCache::open(&db_path, DEFAULT_MAX_SIZE_MB, TEST_SHAPE);
        assert_eq!(
            reopened.get(key, 3, 9).as_deref(),
            Some(&b"legacy json payload"[..])
        );
    }

    #[test]
    fn save_uses_binary_format() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("t.lbug");
        let key = ResponseCache::key("hub_nodes", &json!({"n": 5}));
        {
            let mut cache = ResponseCache::open(&db_path, DEFAULT_MAX_SIZE_MB, TEST_SHAPE);
            cache.insert(key, "hub_nodes", b"save delegates to flush", 1, 1);
            cache.save();
        }
        let raw = std::fs::read(ResponseCache::sidecar_path(&db_path)).unwrap();
        assert!(
            raw.starts_with(b"NWRC"),
            "save() should write binary format"
        );

        let mut reopened = ResponseCache::open(&db_path, DEFAULT_MAX_SIZE_MB, TEST_SHAPE);
        assert_eq!(
            reopened.get(key, 1, 1).as_deref(),
            Some(&b"save delegates to flush"[..])
        );
    }

    /// H1: an entry written under one response-shape version must never be
    /// served under another, even though generation, scope digest and TTL all
    /// still agree. This is the upgrade case: same graph, newer binary.
    #[test]
    fn entry_written_under_another_shape_version_is_never_served() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("t.lbug");
        let key = ResponseCache::key("brain_search", &json!({"query": "q"}));
        let old_shape = b"{\"query\":\"q\"}";

        // The "old binary" writes an entry and persists it.
        {
            let mut cache = ResponseCache::open(&db_path, DEFAULT_MAX_SIZE_MB, TEST_SHAPE);
            cache.insert(key, "brain_search", old_shape, 5, 99);
            cache.save();
            // Sanity: under its OWN shape version it is a hit, so the miss
            // below is attributable to the shape version and nothing else.
            assert_eq!(cache.get(key, 5, 99).as_deref(), Some(&old_shape[..]));
        }

        // The "new binary" — identical generation (5) and scope digest (99),
        // well inside the TTL — must not see it.
        let mut upgraded = ResponseCache::open(&db_path, DEFAULT_MAX_SIZE_MB, TEST_SHAPE + 1);
        assert_eq!(
            upgraded.get(key, 5, 99),
            None,
            "a pre-upgrade response shape must not survive a shape-version change"
        );
        assert_eq!(
            upgraded.len(),
            0,
            "foreign-shape entries must be dropped on open, not merely skipped"
        );

        // And the old binary can still read its own entry: the sidecar is
        // partitioned by shape version, not destroyed by the newer reader
        // (which has not written yet).
        let mut original = ResponseCache::open(&db_path, DEFAULT_MAX_SIZE_MB, TEST_SHAPE);
        assert_eq!(original.get(key, 5, 99).as_deref(), Some(&old_shape[..]));
    }

    /// Defense in depth: `open` filters, but an in-memory entry whose shape
    /// version differs must also miss in `get`.
    #[test]
    fn in_memory_shape_mismatch_misses() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("t.lbug");
        let mut cache = ResponseCache::open(&db_path, DEFAULT_MAX_SIZE_MB, TEST_SHAPE);
        let key = ResponseCache::key("brain_search", &json!({}));
        cache.insert(key, "brain_search", b"payload", 1, 1);
        assert!(cache.get(key, 1, 1).is_some());
        if let Some(e) = cache.entries.get_mut(&key) {
            e.shape_version = TEST_SHAPE ^ 0xFFFF;
        }
        assert_eq!(cache.get(key, 1, 1), None);
    }

    /// The entry layout every pre-`shape_version` binary wrote: the fields of
    /// [`CacheEntry`] as they stood before H1, in declaration order. Serializing
    /// THIS is what makes the test below a real upgrade test — it produces the
    /// 7-element MessagePack array an old binary actually left on disk, rather
    /// than a new-format entry with a zeroed field.
    #[derive(Serialize)]
    struct LegacyCacheEntry {
        key_hash: u64,
        tool: String,
        response: Vec<u8>,
        created_at: f64,
        generation: u64,
        scope_digest: u64,
        last_access: f64,
    }

    #[derive(Serialize)]
    struct LegacyCacheDoc {
        entries: Vec<LegacyCacheEntry>,
    }

    /// The real-world upgrade path: every live sidecar is binary `NWRC`
    /// (MessagePack + ZSTD), not JSON. A binary sidecar written by a
    /// pre-`shape_version` release must not be served.
    ///
    /// This also pins the decode behavior the field's doc comment relies on:
    /// the shorter 7-element array does NOT fail to decode — `#[serde(default)]`
    /// fills the missing trailing element with `0` — so the guarantee comes
    /// from `open()` filtering on `shape_version`, not from a decode error.
    #[test]
    fn pre_shape_version_binary_sidecar_is_dropped_on_open() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("t.lbug");
        let key = ResponseCache::key("brain_search", &json!({"query": "q"}));
        let payload = crate::zstd::encode_all(&b"{\"query\":\"q\"}"[..], ZSTD_LEVEL).unwrap();

        let doc = LegacyCacheDoc {
            entries: vec![LegacyCacheEntry {
                key_hash: key,
                tool: "brain_search".to_string(),
                response: payload,
                created_at: now_secs(),
                generation: 5,
                scope_digest: 99,
                last_access: now_secs(),
            }],
        };
        // Exactly how the old binary encoded it: MessagePack → ZSTD → NWRC.
        let msgpack = rmp_serde::to_vec(&doc).unwrap();
        let compressed = crate::zstd::encode_all(msgpack.as_slice(), ZSTD_LEVEL).unwrap();
        let mut bytes = Vec::new();
        bytes.extend_from_slice(CACHE_MAGIC);
        bytes.push(CACHE_VERSION);
        bytes.extend_from_slice(&compressed);
        std::fs::write(ResponseCache::sidecar_path(&db_path), &bytes).unwrap();

        // The document itself still decodes — assert that directly so a future
        // change to serde/rmp behavior is caught here rather than silently
        // turning this test into a decode-failure test that proves nothing.
        let decoded = decode_cache_bytes(&bytes).expect("legacy 7-field entry must still decode");
        assert_eq!(decoded.entries.len(), 1);
        assert_eq!(
            decoded.entries[0].shape_version, 0,
            "a missing trailing field must default to 0, not error"
        );

        // And the cache must still refuse to serve it.
        let mut cache = ResponseCache::open(&db_path, DEFAULT_MAX_SIZE_MB, TEST_SHAPE);
        assert_eq!(
            cache.len(),
            0,
            "a pre-shape_version binary sidecar must be dropped on open"
        );
        assert_eq!(cache.get(key, 5, 99), None);
    }

    /// The same guarantee on the legacy JSON path.
    #[test]
    fn pre_shape_version_sidecar_is_not_served() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("t.lbug");
        let key = ResponseCache::key("brain_search", &json!({}));
        let legacy = serde_json::json!({
            "entries": [{
                "key_hash": key,
                "tool": "brain_search",
                // zstd of b"{}" is not needed: the entry must miss before any
                // decompression happens.
                "response": [],
                "created_at": now_secs(),
                "generation": 5u64,
                "scope_digest": 99u64,
                "last_access": now_secs(),
            }]
        });
        std::fs::write(
            ResponseCache::sidecar_path(&db_path),
            serde_json::to_vec(&legacy).unwrap(),
        )
        .unwrap();

        let mut cache = ResponseCache::open(&db_path, DEFAULT_MAX_SIZE_MB, TEST_SHAPE);
        assert_eq!(cache.len(), 0, "legacy entries decode as shape_version 0");
        assert_eq!(cache.get(key, 5, 99), None);
    }

    #[test]
    fn scope_digest_is_order_independent() {
        let a = scope_digest_from_hashes(vec![("a.rs", "h1"), ("b.rs", "h2")]);
        let b = scope_digest_from_hashes(vec![("b.rs", "h2"), ("a.rs", "h1")]);
        assert_eq!(a, b);
        let c = scope_digest_from_hashes(vec![("a.rs", "h1"), ("b.rs", "CHANGED")]);
        assert_ne!(a, c);
    }
}
