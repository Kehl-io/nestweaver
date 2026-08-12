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
    /// equality (H1). Entries from a sidecar written before this field existed
    /// decode as `0` (JSON) or fail the whole document (MessagePack, which
    /// encodes structs positionally) — both outcomes are safe, since a real
    /// shape version is never `0`.
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
        zstd::decode_all(entry.response.as_slice()).ok()
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
        let compressed = match zstd::encode_all(response, ZSTD_LEVEL) {
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
    /// Uses atomic write: serializes to a `.tmp` file then renames to the final
    /// path to prevent corruption on crash. Best-effort; logs on failure.
    pub fn flush(&self) {
        match self.encode_binary() {
            Ok(bytes) => {
                let tmp = self.path.with_extension("cache.tmp");
                if let Err(e) = std::fs::write(&tmp, &bytes) {
                    tracing::warn!("response cache: failed to write tmp sidecar: {e}");
                    return;
                }
                if let Err(e) = std::fs::rename(&tmp, &self.path) {
                    tracing::warn!("response cache: failed to rename sidecar: {e}");
                    let _ = std::fs::remove_file(&tmp);
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
        let compressed = zstd::encode_all(msgpack.as_slice(), ZSTD_LEVEL)?;
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
        let decompressed = zstd::decode_all(payload)?;
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

    /// A sidecar written before `shape_version` existed must not be served.
    /// The JSON legacy path decodes it with `shape_version: 0`; a real binary
    /// never uses 0, so it misses.
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
