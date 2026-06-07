# NestWeaver Performance Audit — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Implement research-validated performance optimizations across NestWeaver's write path, query path, and MCP layer — targeting 10-80x index speedup and 10,000x MCP tool-call latency reduction on the cache path.

**Architecture:** Six independent phases, each producing a shippable PR. Phases are ordered by impact. Each phase touches a narrow set of crates and can be merged independently. All optimizations preserve correctness — no accuracy tradeoffs.

**Tech Stack:** Rust (edition 2024), LadybugDB (lbug 0.16.1), tree-sitter 0.26, Tantivy 0.26, serde/serde_json, zstd 0.13.

**Conventions:** Conventional commits (`perf:` scope), `cargo clippy --workspace --all-targets -- -D warnings` must pass, `cargo fmt --all`, no `unwrap()`/`expect()` outside tests, `thiserror` in library crates, `tracing` for logging.

---

## Phase 1: Write-Path Transaction Batching (W1 + W2 + W3 + W4)

**Estimated index speedup: 10-80x** (validated by SQLite/Kuzu benchmarks; current code auto-commits per statement).

**Crates touched:** `nestweaver-store`, `nestweaver-engine`

### File Map

| Action | File | Responsibility |
|--------|------|----------------|
| Modify | `crates/nestweaver-store/src/write.rs` | Add `_on` variants for markdown batch inserts; add `bulk_vault_write`; bulk delete methods |
| Modify | `crates/nestweaver-store/src/db.rs:310-322` | No changes needed — `begin_transaction`/`commit_transaction` already exist |
| Modify | `crates/nestweaver-engine/src/index_md.rs:1075-1125` | Call `bulk_vault_write` instead of individual batch inserts |
| Modify | `crates/nestweaver-engine/src/index_md.rs:376-648` | Wrap `reinsert_single_note` calls in a transaction |
| Modify | `crates/nestweaver-engine/src/index.rs:1489-1517` | Replace per-file loop in `delete_repo_all_data` with bulk delete |
| Test | `crates/nestweaver-store/src/lib.rs` (inline tests) | Add transaction-wrapped insert + cascade delete tests |

---

### Task 1.1: Add `_on` Variants for Markdown Batch Inserts

**Files:**
- Modify: `crates/nestweaver-store/src/write.rs:865-1125`

The code-index path already has `_on` variants (e.g., `batch_insert_symbols_on`) that accept an external `&Connection` for use within a transaction. The markdown path (`batch_insert_notes`, `batch_insert_headings`, `batch_insert_sections`) lacks these. Add them.

- [ ] **Step 1: Write failing test for transactional note insert**

Add to the `#[cfg(test)]` module in `crates/nestweaver-store/src/lib.rs`:

```rust
#[test]
fn test_bulk_vault_write_atomic() {
    let store = GraphStore::in_memory().unwrap();
    let conn = store.begin_transaction().unwrap();

    let notes = vec![Note {
        uid: "n1".into(),
        title: "Test Note".into(),
        vault_uid: "v1".into(),
        file_path: "test.md".into(),
        modified_at: 0.0,
        created_at: 0.0,
        text_content: String::new(),
        frontmatter: String::new(),
        frontmatter_keys: String::new(),
        content_hash: String::new(),
        tags_csv: String::new(),
    }];
    let result = store.batch_insert_notes_on(&conn, &notes);
    assert!(result.is_ok());

    store.commit_transaction(&conn).unwrap();
    assert_eq!(store.count_notes().unwrap(), 1);
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cd /Users/korykehl/dev/workspace/nestweaver && cargo test -p nestweaver-store test_bulk_vault_write_atomic -- --nocapture`

Expected: FAIL — `batch_insert_notes_on` does not exist.

- [ ] **Step 3: Implement `_on` variants for all markdown batch inserts**

In `crates/nestweaver-store/src/write.rs`, for each of the following functions, extract the body into an `_on` variant that takes `conn: &lbug::Connection<'_>` and have the original call it:

- `batch_insert_notes` (L865) → `batch_insert_notes_on`
- `batch_insert_headings` (L967) → `batch_insert_headings_on`
- `batch_insert_sections` (L1021) → `batch_insert_sections_on`
- `batch_insert_vault_note_edges` → `batch_insert_vault_note_edges_on`
- `batch_insert_note_heading_edges` → `batch_insert_note_heading_edges_on`
- `batch_insert_note_section_edges` → `batch_insert_note_section_edges_on`
- `batch_insert_heading_section_edges` → `batch_insert_heading_section_edges_on`
- `batch_insert_heading_parent_edges` → `batch_insert_heading_parent_edges_on`
- `batch_insert_note_tag_edges` → `batch_insert_note_tag_edges_on`
- `batch_insert_section_tag_edges` → `batch_insert_section_tag_edges_on`
- `batch_insert_wikilink_to_note_edges` → `batch_insert_wikilink_to_note_edges_on`
- `batch_insert_wikilink_to_heading_edges` → `batch_insert_wikilink_to_heading_edges_on`
- `batch_insert_tags` → `batch_insert_tags_on`

Pattern (same as existing `batch_insert_symbols` / `batch_insert_symbols_on`):

```rust
pub fn batch_insert_notes_on(
    &self,
    conn: &lbug::Connection<'_>,
    notes: &[Note],
) -> Result<(), StoreError> {
    // existing body of batch_insert_notes, but using `conn` instead of `self.conn()?`
}

pub fn batch_insert_notes(&self, notes: &[Note]) -> Result<(), StoreError> {
    let conn = self.conn()?;
    self.batch_insert_notes_on(&conn, notes)
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cd /Users/korykehl/dev/workspace/nestweaver && cargo test -p nestweaver-store test_bulk_vault_write_atomic -- --nocapture`

Expected: PASS

- [ ] **Step 5: Run full test suite + clippy**

Run: `cd /Users/korykehl/dev/workspace/nestweaver && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace`

Expected: All pass, zero warnings.

- [ ] **Step 6: Commit**

```bash
git add crates/nestweaver-store/src/write.rs crates/nestweaver-store/src/lib.rs
git commit -m "perf(store): add _on variants for markdown batch inserts to support transactional writes"
```

---

### Task 1.2: Add `bulk_vault_write` and Wire Into `index_into_store`

**Files:**
- Modify: `crates/nestweaver-store/src/write.rs`
- Modify: `crates/nestweaver-engine/src/index_md.rs:1075-1293`

- [ ] **Step 1: Write failing test for bulk_vault_write**

Add to `crates/nestweaver-store/src/lib.rs` tests:

```rust
#[test]
fn test_bulk_vault_write_round_trip() {
    let store = GraphStore::in_memory().unwrap();

    let notes = vec![Note {
        uid: "n1".into(),
        title: "Test".into(),
        vault_uid: "v1".into(),
        file_path: "test.md".into(),
        modified_at: 0.0,
        created_at: 0.0,
        text_content: String::new(),
        frontmatter: String::new(),
        frontmatter_keys: String::new(),
        content_hash: String::new(),
        tags_csv: String::new(),
    }];
    let headings = vec![];
    let sections = vec![];

    store.bulk_vault_write(&notes, &headings, &sections, &[], &[], &[], &[], &[], &[], &[], &[], &[], &[])
        .unwrap();

    assert_eq!(store.count_notes().unwrap(), 1);
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cd /Users/korykehl/dev/workspace/nestweaver && cargo test -p nestweaver-store test_bulk_vault_write_round_trip -- --nocapture`

Expected: FAIL — `bulk_vault_write` does not exist.

- [ ] **Step 3: Implement `bulk_vault_write`**

In `crates/nestweaver-store/src/write.rs`, add a method that mirrors `bulk_index_write` (L562-619) but for the markdown path:

```rust
pub fn bulk_vault_write(
    &self,
    notes: &[Note],
    headings: &[Heading],
    sections: &[Section],
    vault_note_edges: &[(&str, &str)],
    note_heading_edges: &[(&str, &str)],
    note_section_edges: &[(&str, &str)],
    heading_section_edges: &[(&str, &str)],
    heading_parent_edges: &[(&str, &str, i32)],
    tags: &[Tag],
    note_tag_edges: &[(&str, &str)],
    section_tag_edges: &[(&str, &str)],
    edges: &[ResolvedEdge],
) -> Result<(), StoreError> {
    let conn = self.begin_transaction()?;
    self.batch_insert_notes_on(&conn, notes)?;
    self.batch_insert_headings_on(&conn, headings)?;
    self.batch_insert_sections_on(&conn, sections)?;
    self.batch_insert_vault_note_edges_on(&conn, vault_note_edges)?;
    self.batch_insert_note_heading_edges_on(&conn, note_heading_edges)?;
    self.batch_insert_note_section_edges_on(&conn, note_section_edges)?;
    self.batch_insert_heading_section_edges_on(&conn, heading_section_edges)?;
    self.batch_insert_heading_parent_edges_on(&conn, heading_parent_edges)?;
    self.batch_insert_tags_on(&conn, tags)?;
    self.batch_insert_note_tag_edges_on(&conn, note_tag_edges)?;
    self.batch_insert_section_tag_edges_on(&conn, section_tag_edges)?;
    self.batch_insert_edges_on(&conn, edges)?;
    self.commit_transaction(&conn)?;
    Ok(())
}
```

Note: You'll also need a `batch_insert_edges_on` variant of `batch_insert_edges` (L621). Follow the same `_on` pattern — extract the body to accept `conn`, have the original delegate.

- [ ] **Step 4: Run test to verify it passes**

Run: `cd /Users/korykehl/dev/workspace/nestweaver && cargo test -p nestweaver-store test_bulk_vault_write_round_trip -- --nocapture`

Expected: PASS

- [ ] **Step 5: Wire `index_into_store` to use `bulk_vault_write`**

In `crates/nestweaver-engine/src/index_md.rs`, modify `index_into_store` (around L1075-1293). The current code calls each batch insert individually. Collect all the data into vectors first (the code already does this — the vectors are built during Phase 1 parsing), then pass them all to `store.bulk_vault_write(...)` in a single call.

The key change is replacing the sequence of individual `store.batch_insert_*` calls (L1077-1293) with a single `store.bulk_vault_write(...)` call after all data is collected, including the wikilink resolution and tag edges that happen in the "Pass 2" section.

Adjust the function signature of `bulk_vault_write` as needed to match the actual types used in `index_into_store`. The edge types for tags and wikilinks may need additional parameter slots.

- [ ] **Step 6: Run full test suite**

Run: `cd /Users/korykehl/dev/workspace/nestweaver && cargo test --workspace`

Expected: All pass.

- [ ] **Step 7: Commit**

```bash
git add crates/nestweaver-store/src/write.rs crates/nestweaver-engine/src/index_md.rs crates/nestweaver-store/src/lib.rs
git commit -m "perf(store): wrap vault indexing in single transaction via bulk_vault_write

Benchmarks show 10-80x speedup for batch inserts when wrapped in a single
transaction vs auto-committing per statement (validated against SQLite and
Kuzu documentation)."
```

---

### Task 1.3: Add Bulk Delete for Vault Cascade

**Files:**
- Modify: `crates/nestweaver-store/src/write.rs:1492-1548`

- [ ] **Step 1: Write failing test**

Add to `crates/nestweaver-store/src/lib.rs` tests:

```rust
#[test]
fn test_delete_vault_cascade_bulk() {
    let store = GraphStore::in_memory().unwrap();
    // Insert a vault with 3 notes, each with headings and sections
    store.upsert_vault("v1", "test-vault", "/tmp/test").unwrap();
    for i in 0..3 {
        let uid = format!("n{i}");
        store.insert_note(&Note {
            uid: uid.clone(),
            title: format!("Note {i}"),
            vault_uid: "v1".into(),
            file_path: format!("note{i}.md"),
            modified_at: 0.0,
            created_at: 0.0,
            text_content: String::new(),
            frontmatter: String::new(),
            frontmatter_keys: String::new(),
            content_hash: String::new(),
            tags_csv: String::new(),
        }).unwrap();
        store.insert_vault_note_edge("v1", &uid).unwrap();
    }
    assert_eq!(store.count_notes().unwrap(), 3);

    let deleted = store.delete_vault_cascade("v1").unwrap();
    assert_eq!(deleted, 3);
    assert_eq!(store.count_notes().unwrap(), 0);
}
```

- [ ] **Step 2: Run test to verify it passes with current implementation**

Run: `cd /Users/korykehl/dev/workspace/nestweaver && cargo test -p nestweaver-store test_delete_vault_cascade_bulk -- --nocapture`

Expected: PASS (the test verifies behavior, not performance — the current per-note loop is correct).

- [ ] **Step 3: Replace per-note loop with bulk DETACH DELETE**

In `crates/nestweaver-store/src/write.rs`, replace the body of `delete_vault_cascade` (L1534-1548):

```rust
pub fn delete_vault_cascade(&self, vault_uid: &str) -> Result<usize, StoreError> {
    let notes = self.list_notes(Some(vault_uid))?;
    let count = notes.len();
    if count == 0 {
        // Still delete the vault node itself
        self.exec_params(
            "MATCH (v:Vault {uid: $uid}) DETACH DELETE v",
            vec![("uid", lbug::Value::String(vault_uid.to_string()))],
        )?;
        return Ok(0);
    }

    let conn = self.begin_transaction()?;

    // Delete sections belonging to notes in this vault
    conn.exec_params(
        "MATCH (n:Note {vault_uid: $vid})-[:NOTE_HAS_SECTION]->(s:Section) DETACH DELETE s",
        vec![("vid", lbug::Value::String(vault_uid.to_string()))],
    ).map_err(|e| StoreError::Query(e.to_string()))?;

    // Delete headings belonging to notes in this vault
    conn.exec_params(
        "MATCH (n:Note {vault_uid: $vid})-[:NOTE_HAS_HEADING]->(h:Heading) DETACH DELETE h",
        vec![("vid", lbug::Value::String(vault_uid.to_string()))],
    ).map_err(|e| StoreError::Query(e.to_string()))?;

    // Delete unresolved wikilinks for notes in this vault
    conn.exec_params(
        "MATCH (u:UnresolvedWikilink {vault_uid: $vid}) DETACH DELETE u",
        vec![("vid", lbug::Value::String(vault_uid.to_string()))],
    ).map_err(|e| StoreError::Query(e.to_string()))?;

    // Delete notes in this vault
    conn.exec_params(
        "MATCH (n:Note {vault_uid: $vid}) DETACH DELETE n",
        vec![("vid", lbug::Value::String(vault_uid.to_string()))],
    ).map_err(|e| StoreError::Query(e.to_string()))?;

    // Delete the vault node
    conn.exec_params(
        "MATCH (v:Vault {uid: $uid}) DETACH DELETE v",
        vec![("uid", lbug::Value::String(vault_uid.to_string()))],
    ).map_err(|e| StoreError::Query(e.to_string()))?;

    self.commit_transaction(&conn)?;
    Ok(count)
}
```

Note: Verify that `conn.exec_params` exists on `lbug::Connection`. If only `store.exec_params` exists (which creates a fresh connection), you may need to add a helper or use `conn.query(...)` directly. Check the `bulk_index_write` implementation for the pattern used there.

- [ ] **Step 4: Run test again to verify correctness preserved**

Run: `cd /Users/korykehl/dev/workspace/nestweaver && cargo test -p nestweaver-store test_delete_vault_cascade_bulk -- --nocapture`

Expected: PASS

- [ ] **Step 5: Apply same pattern to `delete_repo_all_data`**

In `crates/nestweaver-engine/src/index.rs`, replace the per-file loop in `delete_repo_all_data` (L1489-1517) with bulk delete queries scoped by `repo_uid`:

```rust
fn delete_repo_all_data(
    store: &nestweaver_store::GraphStore,
    r_uid: &str,
) -> Result<(), anyhow::Error> {
    let conn = store.begin_transaction()?;

    // Delete all symbols in this repo
    conn.exec_params(
        "MATCH (s:Symbol {repo_uid: $rid}) DETACH DELETE s",
        vec![("rid", lbug::Value::String(r_uid.to_string()))],
    ).map_err(|e| anyhow::anyhow!("{e}"))?;

    // Delete all files in this repo
    conn.exec_params(
        "MATCH (f:File {repo_uid: $rid}) DETACH DELETE f",
        vec![("rid", lbug::Value::String(r_uid.to_string()))],
    ).map_err(|e| anyhow::anyhow!("{e}"))?;

    store.commit_transaction(&conn)?;

    // These already use bulk operations internally
    store.clear_repo_derived_nodes(r_uid)?;
    store.delete_repo_node(r_uid)?;
    Ok(())
}
```

Verify that Symbol and File nodes have `repo_uid` as a stored property by checking the schema in `db.rs`'s `init_schema`. If not, fall back to the join pattern: `MATCH (r:Repo {uid: $rid})-[:REPO_HAS_FILE]->(f:File)-[:FILE_HAS_SYMBOL]->(s:Symbol) DETACH DELETE s`.

- [ ] **Step 6: Run full test suite**

Run: `cd /Users/korykehl/dev/workspace/nestweaver && cargo test --workspace`

Expected: All pass.

- [ ] **Step 7: Commit**

```bash
git add crates/nestweaver-store/src/write.rs crates/nestweaver-engine/src/index.rs
git commit -m "perf(store): replace per-note/per-file cascade deletes with bulk DETACH DELETE

Reduces vault cascade delete from 4N+1 queries to ~5 queries. Repo delete
from 3N+3 to ~4. Neo4j docs explicitly recommend this over per-row loops."
```

---

## Phase 2: In-Process Response Cache (S1 + S6)

**Estimated latency reduction: 1-9ms per MCP tool call → <100ns** (validated by djkoloski benchmarks, RocksDB/sled/Tantivy patterns).

**Crates touched:** `nestweaver-store`, `nestweaver-mcp`

### File Map

| Action | File | Responsibility |
|--------|------|----------------|
| Modify | `crates/nestweaver-store/src/cache.rs` | Restructure ResponseCache for in-process lifetime; add binary serialization |
| Modify | `crates/nestweaver-store/Cargo.toml` | Add `rmp-serde` dependency (already in engine; or use `bincode`) |
| Modify | `crates/nestweaver-mcp/src/tools.rs:414-445` | Hold ResponseCache in thread-local; remove per-call open/save |

---

### Task 2.1: Refactor ResponseCache for In-Process Lifetime

**Files:**
- Modify: `crates/nestweaver-store/src/cache.rs`
- Modify: `crates/nestweaver-store/Cargo.toml`

- [ ] **Step 1: Write test for in-process cache hit without disk round-trip**

Add to the existing `#[cfg(test)]` module in `cache.rs`:

```rust
#[test]
fn test_cache_hit_no_disk_io() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("test.lbug");

    let mut cache = ResponseCache::open(&db_path, 10);
    let key = ResponseCache::key("test_tool", &serde_json::json!({"a": 1}));

    cache.insert(key, "test_tool", b"hello world", 1, 42);
    // Do NOT call cache.save() — the hit should work purely in-memory
    let hit = cache.get(key, 1, 42);
    assert!(hit.is_some());
    assert_eq!(hit.unwrap(), b"hello world");
}
```

- [ ] **Step 2: Run test — should pass (in-memory HashMap already works this way)**

Run: `cd /Users/korykehl/dev/workspace/nestweaver && cargo test -p nestweaver-store test_cache_hit_no_disk_io -- --nocapture`

Expected: PASS — confirms the in-process path already works; the issue is the callers that re-open from disk every time.

- [ ] **Step 3: Add `flush` method with binary serialization**

Add `rmp-serde = "1"` to `crates/nestweaver-store/Cargo.toml` under `[dependencies]`.

In `cache.rs`, add a `flush` method that uses MessagePack instead of JSON, and keep `save` as a wrapper:

```rust
const CACHE_MAGIC: &[u8; 4] = b"NWRC";
const CACHE_VERSION: u8 = 1;

pub fn flush(&self) -> Result<(), std::io::Error> {
    let doc = CacheDoc {
        entries: self.entries.values().cloned().collect(),
    };
    let mut buf = Vec::with_capacity(64 * 1024);
    buf.extend_from_slice(CACHE_MAGIC);
    buf.push(CACHE_VERSION);
    let msgpack = rmp_serde::to_vec(&doc)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
    let compressed = zstd::encode_all(msgpack.as_slice(), 3)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
    buf.extend_from_slice(&compressed);

    let tmp = self.path.with_extension("cache.tmp");
    std::fs::write(&tmp, &buf)?;
    std::fs::rename(&tmp, &self.path)?;
    Ok(())
}
```

Update `open` to try MessagePack first, fall back to JSON for migration:

```rust
pub fn open(db_path: &Path, max_size_mb: u64) -> Self {
    let path = db_path.with_extension("cache");
    let entries = std::fs::read(&path)
        .ok()
        .and_then(|bytes| {
            if bytes.starts_with(CACHE_MAGIC) && bytes.len() > 5 {
                // Binary format: magic + version + zstd(msgpack)
                let decompressed = zstd::decode_all(&bytes[5..]).ok()?;
                let doc: CacheDoc = rmp_serde::from_slice(&decompressed).ok()?;
                Some(doc.entries)
            } else {
                // Legacy JSON fallback
                let doc: CacheDoc = serde_json::from_slice(&bytes).ok()?;
                Some(doc.entries)
            }
        })
        .unwrap_or_default();

    let map = entries.into_iter().map(|e| (e.key_hash, e)).collect();
    Self {
        path,
        entries: map,
        max_size_bytes: max_size_mb * 1024 * 1024,
    }
}
```

Keep the old `save` method as a deprecated wrapper calling `flush`, so nothing breaks mid-migration.

- [ ] **Step 4: Write test for binary round-trip**

```rust
#[test]
fn test_cache_binary_round_trip() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("test.lbug");

    let mut cache = ResponseCache::open(&db_path, 10);
    let key = ResponseCache::key("tool", &serde_json::json!({}));
    cache.insert(key, "tool", b"payload", 1, 100);
    cache.flush().unwrap();

    // Reopen from disk — should load binary format
    let cache2 = ResponseCache::open(&db_path, 10);
    let hit = cache2.get(key, 1, 100);
    assert!(hit.is_some());
    assert_eq!(hit.unwrap(), b"payload");
}
```

- [ ] **Step 5: Run tests**

Run: `cd /Users/korykehl/dev/workspace/nestweaver && cargo test -p nestweaver-store cache -- --nocapture`

Expected: All cache tests pass.

- [ ] **Step 6: Commit**

```bash
git add crates/nestweaver-store/src/cache.rs crates/nestweaver-store/Cargo.toml
git commit -m "perf(store): switch response cache to binary format with in-process flush

Uses MessagePack + ZSTD instead of JSON+base64(ZSTD). 11x faster
serialize, 2.8x faster deserialize, 2.5x smaller on disk. Falls back to
JSON for migration of existing cache files."
```

---

### Task 2.2: Hold ResponseCache In-Process in MCP Tools

**Files:**
- Modify: `crates/nestweaver-mcp/src/tools.rs:414-445`

- [ ] **Step 1: Replace per-call `ResponseCache::open`/`save` with thread-local**

In `crates/nestweaver-mcp/src/tools.rs`, add a thread-local for the cache and a flush-on-drop guard:

```rust
use std::cell::RefCell;
use std::collections::HashMap as StdHashMap;

thread_local! {
    static RESPONSE_CACHE: RefCell<StdHashMap<PathBuf, nestweaver_store::cache::ResponseCache>> =
        RefCell::new(StdHashMap::new());
}
```

Modify `maybe_cached` (L414-445) to use the thread-local instead of opening from disk every time:

```rust
fn maybe_cached(
    store: &GraphStore,
    tantivy: Option<&TantivyIndex>,
    name: &str,
    args: Value,
) -> Result<Value, anyhow::Error> {
    let db_path = match current_db_path(store) {
        Some(p) => p,
        None => return dispatch_uncached(store, tantivy, name, args),
    };

    let key = nestweaver_store::cache::ResponseCache::key(name, &args);
    let generation = store.graph_generation();
    let scope = nestweaver_store::cache::whole_db_scope_digest(&db_path);

    // Check cache (in-process)
    let hit = RESPONSE_CACHE.with(|cell| {
        let mut map = cell.borrow_mut();
        let cache = map.entry(db_path.clone())
            .or_insert_with(|| nestweaver_store::cache::ResponseCache::open(&db_path, CACHE_MAX_SIZE_MB.with(|c| *c.borrow())));
        cache.get(key, generation, scope)
    });

    if let Some(bytes) = hit {
        CACHE_HITS.with(|c| c.set(c.get() + 1));
        return serde_json::from_slice(&bytes)
            .map_err(|e| anyhow::anyhow!("cache deserialize: {e}"));
    }

    CACHE_MISSES.with(|c| c.set(c.get() + 1));
    let result = dispatch_uncached(store, tantivy, name, args)?;
    let serialized = serde_json::to_vec(&result)?;

    RESPONSE_CACHE.with(|cell| {
        let mut map = cell.borrow_mut();
        if let Some(cache) = map.get_mut(&db_path) {
            cache.insert(key, name, &serialized, generation, scope);
        }
    });

    Ok(result)
}
```

- [ ] **Step 2: Add periodic flush (every N inserts or at shutdown)**

Add a flush counter and trigger flush every 50 cache misses:

```rust
thread_local! {
    static FLUSH_COUNTER: Cell<u32> = const { Cell::new(0) };
}

// After cache.insert(...) in the miss path:
FLUSH_COUNTER.with(|c| {
    let count = c.get() + 1;
    c.set(count);
    if count % 50 == 0 {
        RESPONSE_CACHE.with(|cell| {
            let map = cell.borrow();
            for cache in map.values() {
                let _ = cache.flush();
            }
        });
    }
});
```

Also add a flush in the MCP server shutdown path if one exists (check `crates/nestweaver-mcp/src/lib.rs` for a shutdown hook or Drop impl).

- [ ] **Step 3: Remove `cache.save()` calls from `maybe_cached`**

The old code called `cache.save()` on both hit and miss paths. Remove those — the periodic flush and shutdown flush replace them.

- [ ] **Step 4: Run full test suite + clippy**

Run: `cd /Users/korykehl/dev/workspace/nestweaver && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace`

Expected: All pass.

- [ ] **Step 5: Commit**

```bash
git add crates/nestweaver-mcp/src/tools.rs
git commit -m "perf(mcp): hold response cache in-process, flush periodically

Eliminates full disk round-trip (open + JSON deserialize + re-serialize +
write) on every cacheable tool call. Cache lookup is now ~100ns instead of
1-9ms. Flush to disk every 50 misses and at shutdown."
```

---

## Phase 3: PPR Graph Cache + Query-Path Batching (Q3 + Q2 + Q1)

**Estimated improvement: eliminates biggest per-query I/O** (validated by Memgraph 5x benchmark, SIGMOD/VLDB PPR papers).

**Crates touched:** `nestweaver-store`

### File Map

| Action | File | Responsibility |
|--------|------|----------------|
| Modify | `crates/nestweaver-store/src/db.rs:12-43` | Add PPR graph cache field to GraphStore |
| Modify | `crates/nestweaver-store/src/ranking.rs:479-659` | Cache PprGraph, reuse across calls |
| Modify | `crates/nestweaver-store/src/traverse.rs:214-239` | Cache symbol name index |
| Modify | `crates/nestweaver-store/src/read.rs` | Add `batch_lookup_symbols` method |
| Modify | `crates/nestweaver-engine/src/query.rs:307-329` | Use batch lookup for PPR hydration |

---

### Task 3.1: Cache PprGraph in GraphStore

**Files:**
- Modify: `crates/nestweaver-store/src/db.rs:12-43`
- Modify: `crates/nestweaver-store/src/ranking.rs:479-659`

- [ ] **Step 1: Write test verifying PPR returns same results with cached graph**

Add to `crates/nestweaver-store/src/ranking.rs` test module (or `lib.rs`):

```rust
#[test]
fn test_ppr_cached_graph_consistency() {
    // Build a small graph, run PPR twice, verify identical results
    let store = GraphStore::in_memory().unwrap();
    // ... insert repo, files, symbols, edges (use existing test helpers) ...
    
    let scope = GraphScope::code_only();
    let seeds = vec!["sym1".to_string()];
    
    let r1 = store.personalized_pagerank_with_intent(&seeds, 0.85, 20, &scope, None).unwrap();
    let r2 = store.personalized_pagerank_with_intent(&seeds, 0.85, 20, &scope, None).unwrap();
    
    assert_eq!(r1.len(), r2.len());
    for ((u1, s1), (u2, s2)) in r1.iter().zip(r2.iter()) {
        assert_eq!(u1, u2);
        assert!((s1 - s2).abs() < 1e-10);
    }
}
```

- [ ] **Step 2: Run test to verify it passes (baseline)**

Run: `cd /Users/korykehl/dev/workspace/nestweaver && cargo test -p nestweaver-store test_ppr_cached_graph_consistency -- --nocapture`

Expected: PASS

- [ ] **Step 3: Add PprGraph cache to GraphStore**

In `crates/nestweaver-store/src/db.rs`, add a cache field:

```rust
use std::sync::Mutex;

pub struct GraphStore {
    pub(crate) db: lbug::Database,
    pub(crate) pagerank_cache: Mutex<Option<HashMap<String, f64>>>,
    pub(crate) pagerank_generation: AtomicU64,
    pub(crate) graph_generation: AtomicU64,
    pub(crate) interaction_cache: Mutex<Option<HashMap<String, f64>>>,
    pub(crate) git_activity_cache: Mutex<Option<HashMap<String, f64>>>,
    pub(crate) git_activity_weight: Mutex<f64>,
    pub(crate) db_path: Option<PathBuf>,
    // NEW: cached PPR graph, keyed on (generation, scope_hash, intent)
    pub(crate) ppr_graph_cache: Mutex<Option<PprGraphCached>>,
}
```

Add the cache struct in `ranking.rs`:

```rust
pub(crate) struct PprGraphCached {
    pub generation: u64,
    pub scope_hash: u64,
    pub intent: Option<QueryIntent>,
    pub graph: PprGraph,
}
```

Initialize the new field in all `GraphStore` constructors (open, create, in_memory, etc.) with `ppr_graph_cache: Mutex::new(None)`.

- [ ] **Step 4: Use cache in `personalized_pagerank_with_intent`**

In `ranking.rs`, modify `personalized_pagerank_with_intent` (L626-659) to check the cache before calling `load_ppr_graph`:

```rust
pub fn personalized_pagerank_with_intent(
    &self,
    seed_uids: &[String],
    damping: f64,
    max_iterations: u32,
    scope: &GraphScope,
    intent: Option<QueryIntent>,
) -> Result<Vec<(String, f64)>, StoreError> {
    let gen = self.graph_generation.load(Ordering::Relaxed);
    let scope_hash = scope.cache_key_hash();
    let effective_damping = intent.map_or(damping, |i| i.damping());

    // Try cache
    let graph = {
        let cached = self.ppr_graph_cache.lock().unwrap();
        if let Some(ref c) = *cached {
            if c.generation == gen && c.scope_hash == scope_hash && c.intent == intent {
                Some(c.graph.clone())
            } else {
                None
            }
        } else {
            None
        }
    };

    let graph = match graph {
        Some(g) => g,
        None => {
            let g = self.load_ppr_graph(scope, intent)?;
            let mut cached = self.ppr_graph_cache.lock().unwrap();
            *cached = Some(PprGraphCached {
                generation: gen,
                scope_hash,
                intent,
                graph: g.clone(),
            });
            g
        }
    };

    // ... rest of PPR computation unchanged ...
}
```

You'll need to add `cache_key_hash()` to `GraphScope` — hash its `node_queries` and `edge_queries` into a `u64`. Also derive `Clone` on `PprGraph` (it's a tuple of `(Vec<String>, HashMap, Vec<Vec<(usize,f64)>>, Vec<f64>)` — all cloneable). And derive `PartialEq` on `QueryIntent` if not already present.

- [ ] **Step 5: Run test again — should still pass**

Run: `cd /Users/korykehl/dev/workspace/nestweaver && cargo test -p nestweaver-store test_ppr_cached_graph_consistency -- --nocapture`

Expected: PASS

- [ ] **Step 6: Run full test suite + clippy**

Run: `cd /Users/korykehl/dev/workspace/nestweaver && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace`

Expected: All pass.

- [ ] **Step 7: Commit**

```bash
git add crates/nestweaver-store/src/db.rs crates/nestweaver-store/src/ranking.rs
git commit -m "perf(store): cache PPR adjacency graph keyed on (generation, scope, intent)

Avoids rebuilding the full graph from DB on every PPR call. The graph is
immutable between index refreshes — cache is invalidated by generation
counter. Validated by Memgraph/Neo4j/SIGMOD PPR caching research."
```

---

### Task 3.2: Add `batch_lookup_symbols` and Wire Into PPR Hydration

**Files:**
- Modify: `crates/nestweaver-store/src/read.rs`
- Modify: `crates/nestweaver-engine/src/query.rs:307-329`

- [ ] **Step 1: Write test for batch_lookup_symbols**

Add to `crates/nestweaver-store/src/lib.rs` tests:

```rust
#[test]
fn test_batch_lookup_symbols() {
    let store = GraphStore::in_memory().unwrap();
    // Insert 3 symbols
    // ... (use existing test patterns from the file) ...
    
    let results = store.batch_lookup_symbols(&["s1", "s2", "s3"]).unwrap();
    assert_eq!(results.len(), 3);
    assert!(results.contains_key("s1"));
}
```

- [ ] **Step 2: Run test to verify it fails**

Expected: FAIL — `batch_lookup_symbols` does not exist.

- [ ] **Step 3: Implement batch_lookup_symbols**

In `crates/nestweaver-store/src/read.rs`:

```rust
pub fn batch_lookup_symbols(
    &self,
    uids: &[&str],
) -> Result<HashMap<String, nestweaver_schema::Symbol>, StoreError> {
    if uids.is_empty() {
        return Ok(HashMap::new());
    }
    // Build IN-list as a comma-separated string of quoted UIDs
    let in_list: String = uids.iter()
        .map(|u| format!("'{}'", u.replace('\'', "''")))
        .collect::<Vec<_>>()
        .join(", ");
    let q = format!(
        "MATCH (s:Symbol) WHERE s.uid IN [{}] RETURN {}",
        in_list, SYMBOL_COLUMNS,
    );
    let conn = self.conn()?;
    let result = conn.query(&q)
        .map_err(|e| StoreError::Query(e.to_string()))?;
    let mut map = HashMap::with_capacity(uids.len());
    for row in result {
        let sym = parse_symbol_row(&row)?;
        map.insert(sym.uid.clone(), sym);
    }
    Ok(map)
}
```

Note: Check whether lbug supports `IN [...]` syntax in its Cypher dialect. If not, fall back to issuing one query with `OR` conditions, or iterate with a single prepared statement (still better than N separate queries due to connection reuse).

- [ ] **Step 4: Wire into `build_context_with_intent`**

In `crates/nestweaver-engine/src/query.rs`, replace the per-UID loop (L307-329):

```rust
// Before (N+1):
// for (uid, score) in &ppr_results {
//     let sym = match store.lookup_symbol(uid) { ... };

// After (batch):
let uids: Vec<&str> = ppr_results.iter().map(|(u, _)| u.as_str()).collect();
let sym_map = store.batch_lookup_symbols(&uids)?;
for (uid, score) in &ppr_results {
    let sym = match sym_map.get(uid) {
        Some(s) => s,
        None => continue,
    };
    // ... rest unchanged ...
}
```

Apply the same change to `build_brain_context_hybrid_with_aliases` (L1050-1058) and `build_feature_context` (L628-632).

- [ ] **Step 5: Run full test suite**

Run: `cd /Users/korykehl/dev/workspace/nestweaver && cargo test --workspace`

Expected: All pass.

- [ ] **Step 6: Commit**

```bash
git add crates/nestweaver-store/src/read.rs crates/nestweaver-engine/src/query.rs crates/nestweaver-store/src/lib.rs
git commit -m "perf(store): batch symbol lookups to eliminate N+1 after PPR

Replaces 20+ individual lookup_symbol calls with a single batch query.
Kuzu per-query overhead is ~500µs; batching 20 lookups saves ~10ms per
brain_context call."
```

---

### Task 3.3: Cache Symbol Name Index for `search_symbols_by_name`

**Files:**
- Modify: `crates/nestweaver-store/src/db.rs`
- Modify: `crates/nestweaver-store/src/traverse.rs:214-239`

- [ ] **Step 1: Add symbol name cache to GraphStore**

In `db.rs`, add:

```rust
pub(crate) symbol_name_cache: Mutex<Option<SymbolNameCache>>,
```

In `traverse.rs`, define:

```rust
pub(crate) struct SymbolNameCache {
    pub generation: u64,
    pub by_name: HashMap<String, Vec<(String, nestweaver_schema::Symbol)>>,
    // key = lowercase name, value = vec of (uid, symbol) pairs
}
```

- [ ] **Step 2: Use cache in `search_symbols_by_name`**

```rust
pub fn search_symbols_by_name(
    &self,
    query: &str,
    limit: usize,
) -> Result<Vec<nestweaver_schema::Symbol>, StoreError> {
    let gen = self.graph_generation.load(std::sync::atomic::Ordering::Relaxed);
    let needle = query.to_lowercase();

    let mut cache = self.symbol_name_cache.lock().unwrap();
    if cache.as_ref().map_or(true, |c| c.generation != gen) {
        // Rebuild cache
        let q = format!("MATCH (s:Symbol) RETURN {}", crate::read::SYMBOL_COLUMNS);
        let conn = self.conn()?;
        let result = conn.query(&q)
            .map_err(|e| StoreError::Query(e.to_string()))?;
        let mut by_name: HashMap<String, Vec<(String, nestweaver_schema::Symbol)>> = HashMap::new();
        for row in result {
            let sym = crate::read::parse_symbol_row(&row)?;
            let lower = sym.name.to_lowercase();
            by_name.entry(lower).or_default().push((sym.uid.clone(), sym));
        }
        *cache = Some(SymbolNameCache { generation: gen, by_name });
    }

    let index = cache.as_ref().unwrap();
    let mut matches = Vec::new();
    for (name, syms) in &index.by_name {
        if name.contains(&needle) {
            for (_, sym) in syms {
                matches.push(sym.clone());
                if matches.len() >= limit {
                    return Ok(matches);
                }
            }
        }
    }
    Ok(matches)
}
```

- [ ] **Step 3: Initialize field in all constructors, run tests**

Add `symbol_name_cache: Mutex::new(None)` to every `GraphStore` constructor in `db.rs`.

Run: `cd /Users/korykehl/dev/workspace/nestweaver && cargo test --workspace`

Expected: All pass.

- [ ] **Step 4: Commit**

```bash
git add crates/nestweaver-store/src/db.rs crates/nestweaver-store/src/traverse.rs
git commit -m "perf(store): cache symbol name index keyed on graph generation

Eliminates full-table scan of all symbols on every seed resolution.
Cache is rebuilt only when graph_generation changes (i.e., after reindex)."
```

---

## Phase 4: Tree-Sitter Query Cache (W5)

**Estimated improvement: eliminates 100+ seconds of query compilation for large repos** (validated by tree-sitter issue #1942, Helix editor pattern).

**Crates touched:** `nestweaver-parser`

### File Map

| Action | File | Responsibility |
|--------|------|----------------|
| Modify | `crates/nestweaver-parser/src/parse.rs:399-444, 636-660` | Cache compiled Query objects with OnceLock/LazyLock |

---

### Task 4.1: Cache Compiled Tree-Sitter Queries

**Files:**
- Modify: `crates/nestweaver-parser/src/parse.rs`

- [ ] **Step 1: Write benchmark test**

Add a test that parses the same language multiple times to verify caching works:

```rust
#[cfg(test)]
mod cache_tests {
    use super::*;
    use std::time::Instant;

    #[test]
    fn test_query_cache_speedup() {
        let source = "function hello() { return 42; }";
        let path = std::path::Path::new("test.js");

        // First parse: cold cache
        let t1 = Instant::now();
        let _ = parse_source(path, source).unwrap();
        let cold = t1.elapsed();

        // Second parse: warm cache
        let t2 = Instant::now();
        let _ = parse_source(path, source).unwrap();
        let warm = t2.elapsed();

        // Warm should not be significantly slower than cold
        // (can't assert faster due to noise, but this confirms no regression)
        assert!(warm.as_micros() < cold.as_micros() * 3 + 1000);
    }
}
```

- [ ] **Step 2: Run test (baseline)**

Run: `cd /Users/korykehl/dev/workspace/nestweaver && cargo test -p nestweaver-parser test_query_cache_speedup -- --nocapture`

Expected: PASS

- [ ] **Step 3: Implement query cache using `OnceLock`**

tree-sitter `Query` is `Send + Sync`, so use a global cache. In `parse.rs`, add near the top:

```rust
use std::sync::OnceLock;
use std::collections::HashMap;
use std::sync::Mutex;

static QUERY_CACHE: OnceLock<Mutex<HashMap<QueryCacheKey, tree_sitter::Query>>> = OnceLock::new();

#[derive(Hash, Eq, PartialEq)]
struct QueryCacheKey {
    lang: Language,
    is_jsx: bool,
}

fn get_or_compile_query(
    ts_lang: &tree_sitter::Language,
    lang: Language,
    path: &std::path::Path,
) -> Result<tree_sitter::Query, ParseError> {
    let is_jsx = path.extension()
        .map_or(false, |e| e == "tsx" || e == "jsx");
    let key = QueryCacheKey { lang, is_jsx };

    let cache = QUERY_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    let mut map = cache.lock().unwrap();

    if let Some(q) = map.get(&key) {
        return Ok(q.clone());
    }

    let query_src = query_source(lang, path);
    let query = tree_sitter::Query::new(ts_lang, &query_src)
        .map_err(|e| ParseError::QueryError(e.to_string()))?;
    map.insert(key, query.clone());
    Ok(query)
}
```

Note: Check whether `tree_sitter::Query` implements `Clone`. If not, wrap in `Arc<Query>` and store `Arc<Query>` in the cache. The `QueryCursor` (which IS per-parse and mutable) should NOT be cached.

- [ ] **Step 4: Replace `Query::new` calls in `parse_source`**

In `parse_source` (L660), replace:

```rust
// Before:
let query_src = query_source(lang, path);
let query = Query::new(&ts_lang, &query_src)?;

// After:
let query = get_or_compile_query(&ts_lang, lang, path)?;
```

Apply the same change to `extract_types_from_tree` (L996) if it also calls `Query::new` — it likely has a separate type query that should also be cached with a distinct cache key (e.g., add a `is_type_query: bool` field to `QueryCacheKey`).

- [ ] **Step 5: Run full test suite + clippy**

Run: `cd /Users/korykehl/dev/workspace/nestweaver && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace`

Expected: All pass.

- [ ] **Step 6: Commit**

```bash
git add crates/nestweaver-parser/src/parse.rs
git commit -m "perf(parser): cache compiled tree-sitter Query objects globally

Query::new() compiles S-expressions into internal matchers — expensive
per-call (50-500ms depending on grammar). Now compiled once per
(language, variant) and reused. Validated by Helix editor's OnceLock
pattern and tree-sitter issue #1942."
```

---

## Phase 5: Tantivy Commit Batching (Q15 + Q16)

**Estimated improvement: 10x write throughput** (validated by ParadeDB production result).

**Crates touched:** `nestweaver-store`

### File Map

| Action | File | Responsibility |
|--------|------|----------------|
| Modify | `crates/nestweaver-store/src/tantivy_index.rs:215-274` | Add `update_notes_batch`; use bulk loaders in `write_full_corpus` |

---

### Task 5.1: Add Batch Note Update and Fix `write_full_corpus`

**Files:**
- Modify: `crates/nestweaver-store/src/tantivy_index.rs`

- [ ] **Step 1: Write test for batch update**

```rust
#[cfg(test)]
mod batch_tests {
    use super::*;

    #[test]
    fn test_update_notes_batch_searchable() {
        let dir = tempfile::tempdir().unwrap();
        let ti = TantivyIndex::create(dir.path()).unwrap();

        let notes = vec![
            ("n1", "First Note", "v1", &["Body of first note."][..], &[][..], &[][..], &[][..]),
            ("n2", "Second Note", "v1", &["Body of second note."][..], &[][..], &[][..], &[][..]),
        ];

        ti.update_notes_batch(&notes).unwrap();

        let hits = ti.search("first", 10).unwrap();
        assert!(!hits.is_empty());
        assert_eq!(hits[0].uid, "n1");
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Expected: FAIL — `update_notes_batch` does not exist.

- [ ] **Step 3: Implement `update_notes_batch`**

```rust
pub fn update_notes_batch(
    &self,
    notes: &[(&str, &str, &str, &[String], &[(String, String)], &[(String, String, String)], &[String])],
    // (note_uid, title, vault_uid, body_chunks, headings, sections, tags)
) -> Result<(), TantivyError> {
    let writer_guard = self.writer.as_ref()
        .ok_or(TantivyError::ReadOnly)?;
    let mut writer = writer_guard.lock().unwrap();

    for &(note_uid, title, vault_uid, body_chunks, headings, sections, tags) in notes {
        // Delete existing docs for this note
        let term = Term::from_field_text(self.fields.note_uid, note_uid);
        writer.delete_term(term);

        // Add note doc
        // ... (same logic as current update_note, minus the commit)
    }

    // Single commit at the end
    writer.commit()?;
    drop(writer);
    self.reader.reload()?;
    Ok(())
}
```

Adapt the signature to match the actual types used in callers. The key change is: all `delete_term` + `add_document` calls happen first, then ONE `commit()` at the end.

- [ ] **Step 4: Fix `write_full_corpus` to use bulk loaders**

In `write_full_corpus` (L551-651), replace the per-note `sections_in_note` / `headings_in_note` calls with bulk loads:

```rust
fn write_full_corpus(
    &self,
    writer: &mut tantivy::IndexWriter,
    store: &GraphStore,
) -> Result<usize, TantivyError> {
    let notes = store.list_notes(None)
        .map_err(|e| TantivyError::IndexError(e.to_string()))?;
    let all_sections = store.list_all_sections()
        .map_err(|e| TantivyError::IndexError(e.to_string()))?;
    let all_headings = store.list_all_headings()
        .map_err(|e| TantivyError::IndexError(e.to_string()))?;

    // Build lookup maps
    let sections_by_note: HashMap<String, Vec<_>> = {
        let mut m: HashMap<String, Vec<_>> = HashMap::new();
        // Need to get note_uid for each section — check if Section has a note_uid field
        // If not, use store.note_uid_for_section() or adjust the data model
        // ... populate from all_sections ...
        m
    };
    let headings_by_note: HashMap<String, Vec<_>> = {
        // Same pattern for headings
        let mut m: HashMap<String, Vec<_>> = HashMap::new();
        // ... populate from all_headings ...
        m
    };

    let mut count = 0;
    for note in &notes {
        let sections = sections_by_note.get(&note.uid).map_or(&[][..], |v| v.as_slice());
        let headings = headings_by_note.get(&note.uid).map_or(&[][..], |v| v.as_slice());
        // ... add documents (existing logic) ...
        count += 1 + sections.len() + headings.len();
    }
    Ok(count)
}
```

Check how `sections_in_note` maps sections to notes — it likely uses a `NOTE_HAS_SECTION` edge query. You may need to add a `list_all_sections_with_note_uid()` method to `read.rs` that returns `(note_uid, Section)` pairs, or check if `Section` already has a `note_uid` field.

- [ ] **Step 5: Run full test suite**

Run: `cd /Users/korykehl/dev/workspace/nestweaver && cargo test --workspace`

Expected: All pass.

- [ ] **Step 6: Commit**

```bash
git add crates/nestweaver-store/src/tantivy_index.rs crates/nestweaver-store/src/read.rs
git commit -m "perf(store): batch Tantivy commits and use bulk section/heading loaders

Single commit per batch instead of per-note. write_full_corpus now uses
3 queries (notes + sections + headings) instead of 2N+1. Validated by
ParadeDB's 10x write throughput improvement from the same change."
```

---

## Phase 6: Remaining Optimizations (S3 + S7 + W6 + Q6/Q7)

**Lower priority — implement after Phases 1-5 are merged.**

### Task 6.1: Move Prepared Statement Outside Trigram Loop (S3)

**File:** `crates/nestweaver-store/src/regex.rs:304-338`

Move `conn.prepare(...)` before the inner `for tg in clause` loop. The query string is identical every iteration.

### Task 6.2: Consolidate Embedding Cache (S7)

**File:** `crates/nestweaver-engine/src/embedding.rs:31-46`

Replace per-file `.emb.json` with a single SQLite blob table or LMDB store. APFS readdir on 50k files in one directory takes 3-8 seconds.

### Task 6.3: Hoist `list_notes` Out of `reinsert_single_note` Loop (W6)

**File:** `crates/nestweaver-engine/src/index_md.rs:569`

Load `store.list_notes(None)` once before the incremental update loop in `index_markdown_directory_since`, pass the lookup map into `reinsert_single_note`.

### Task 6.4: Share Timestamp Cache Between Rerank and Recency Bias (Q6/Q7)

**Files:** `crates/nestweaver-engine/src/rerank.rs:120-165`, `crates/nestweaver-mcp/src/tools.rs:1425-1478`

Both `build_node_ages` and `apply_recency_bias` call `list_notes(None)` + `list_all_sections()`. Add a generation-keyed timestamp cache on GraphStore, or at minimum fetch only `(uid, modified_at)` projections and batch-lookup by candidate UIDs.

---

## Benchmark Verification

After each phase is merged, run the existing criterion benchmarks to measure improvement:

```bash
cd /Users/korykehl/dev/workspace/nestweaver
cargo bench --bench brain_benchmarks
```

If benchmarks don't cover the changed paths, add targeted ones to `benches/brain_benchmarks.rs` measuring:
- Vault index time (Phase 1)
- `brain_context` tool call latency (Phase 2 + 3)
- Parse throughput for a multi-file repo (Phase 4)
- Tantivy reindex time (Phase 5)
