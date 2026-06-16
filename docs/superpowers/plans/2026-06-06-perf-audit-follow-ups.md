# Performance Audit Follow-Ups — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Address all review findings from the performance audit PR to eliminate technical debt before merge.

**Architecture:** Six small, independent tasks — each is a focused fix in 1-2 files. No new features, no new dependencies. All tasks are on branch `perf/audit-implementation-plan`.

**Tech Stack:** Rust (edition 2024), LadybugDB (lbug 0.16.1), serde_json, tree-sitter 0.26, Tantivy 0.26.

**Conventions:** Conventional commits (`fix:` scope), `cargo clippy --workspace --all-targets -- -D warnings`, `cargo fmt --all`, no `unwrap()`/`expect()` outside tests, no AI attribution.

---

### Task 1: Use Arc for Symbol Name Cache to Avoid Clone on Hit

**Files:**
- Modify: `crates/nestweaver-store/src/traverse.rs:225-300`
- Modify: `crates/nestweaver-store/src/db.rs` (cache field type)

Currently `search_symbols_by_name` clones the entire `Vec<(String, Symbol)>` out of the Mutex on every cache hit (line 247). For 10k+ symbols this is a non-trivial allocation. Wrapping in `Arc` makes hits a cheap reference-count bump.

- [ ] **Step 1: Change the cache type to use Arc**

In `crates/nestweaver-store/src/db.rs`, change the field type:

```rust
// Before:
pub(crate) symbol_name_cache: Mutex<Option<SymbolNameCached>>,

// After:
pub(crate) symbol_name_cache: Mutex<Option<Arc<SymbolNameCached>>>,
```

Add `use std::sync::Arc;` if not already imported.

In `crates/nestweaver-store/src/traverse.rs`, update `SymbolNameCached` — no changes to the struct itself needed, but update how it's stored and retrieved:

```rust
// In search_symbols_by_name, line ~240-254:
// Before:
let cached_symbols: Option<Vec<(String, nestweaver_schema::Symbol)>> = {
    let guard = self.symbol_name_cache.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(ref c) = *guard {
        if c.generation == cur_gen {
            Some(c.symbols.clone())  // expensive clone
        } else { None }
    } else { None }
};

// After:
let cached_arc: Option<Arc<SymbolNameCached>> = {
    let guard = self.symbol_name_cache.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(ref c) = *guard {
        if c.generation == cur_gen {
            Some(Arc::clone(c))  // cheap ref-count bump
        } else { None }
    } else { None }
};
```

Update the cache-fill path (line ~270-295) to wrap in `Arc::new(...)` when storing.

Update the search loop to iterate `cached_arc.as_ref().unwrap().symbols` instead of the cloned vec.

- [ ] **Step 2: Update all GraphStore constructors**

Change `symbol_name_cache: Mutex::new(None)` — this already works with the new type since `None` is still `Option<Arc<...>>`.

- [ ] **Step 3: Run tests**

Run: `cd /Users/korykehl/dev/workspace/nestweaver && cargo test -p nestweaver-store -- symbol_name_cache`

Expected: Both existing cache tests pass.

- [ ] **Step 4: Run clippy + fmt**

Run: `cd /Users/korykehl/dev/workspace/nestweaver && cargo clippy -p nestweaver-store -- -D warnings && cargo fmt --all`

- [ ] **Step 5: Commit**

```bash
git add crates/nestweaver-store/src/traverse.rs crates/nestweaver-store/src/db.rs
git commit -m "fix(store): use Arc for symbol name cache to avoid full clone on hit"
```

---

### Task 2: Add Length Separators to `scope_hash`

**Files:**
- Modify: `crates/nestweaver-store/src/ranking.rs:343-352`

The `scope_hash` function hashes query strings sequentially without hashing the count of each vec or any separator. Two scopes with different query counts could collide (e.g., `["ab", "c"]` vs `["a", "bc"]`).

- [ ] **Step 1: Add length and separator hashing**

```rust
fn scope_hash(scope: &GraphScope) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    scope.node_queries.len().hash(&mut hasher);
    for q in &scope.node_queries {
        q.hash(&mut hasher);
    }
    scope.edge_queries.len().hash(&mut hasher);
    for eq in &scope.edge_queries {
        eq.query.hash(&mut hasher);
    }
    hasher.finish()
}
```

- [ ] **Step 2: Run tests**

Run: `cd /Users/korykehl/dev/workspace/nestweaver && cargo test -p nestweaver-store -- ppr_graph_cache`

Expected: Both PPR cache tests pass.

- [ ] **Step 3: Clippy + fmt + commit**

```bash
git add crates/nestweaver-store/src/ranking.rs
git commit -m "fix(store): hash vec lengths in scope_hash to prevent prefix collisions"
```

---

### Task 3: Log Daemon Restart Failures Instead of Silencing

**Files:**
- Modify: `src/main.rs` (3 call sites)

Three `let _ = ensure_daemon(...)` calls silently discard spawn failures. Replace with `eprintln!` on error.

- [ ] **Step 1: Fix all three call sites**

Replace each `let _ = ...ensure_daemon(...)` with:

```rust
if let Err(e) = nestweaver_client::autostart::ensure_daemon(&db_path, config_arg) {
    eprintln!("Warning: failed to restart daemon: {e}");
}
```

The three locations are:
1. `src/main.rs:4991-4995` (index restart)
2. `src/main.rs:6079-6083` (brain add restart)
3. `src/main.rs:4734-4739` (materialize-projects restart — pre-existing, fix while here)

Adjust the `config_arg` parameter to match what each call site passes (some pass `config.as_deref().map(Path::new)`, one passes `Some(config.as_path())`, one passes `None`).

- [ ] **Step 2: Run clippy + fmt + commit**

```bash
git add src/main.rs
git commit -m "fix(cli): log daemon restart failures instead of silently discarding"
```

---

### Task 4: Extract Daemon Stop-Restart Helper

> **Superseded (2026-06-16):** The stop-daemon-take-lock pattern introduced here has been replaced by daemon RPC routing. All write operations now go through the daemon's gRPC service. The `stop_daemon_if_running` / `restart_daemon` helpers remain only as a test/CI fallback (`NESTWEAVER_NO_DAEMON=1`). See `docs/superpowers/plans/2026-06-16-daemon-route-all-writes.md`.

**Files:**
- Modify: `src/main.rs`

The daemon stop + poll + restart pattern is duplicated across the index path (~line 4832-4850), brain add path (~line 5986-6004), and materialize-projects path (~line 4700-4713). Extract into a helper.

- [ ] **Step 1: Create helper function**

Add near the other helper functions in `main.rs`:

```rust
/// Stop a running daemon for the given DB path so the caller can acquire
/// the write lock. Returns `true` if a daemon was stopped (and should be
/// restarted after the caller's work is done).
fn stop_daemon_if_running(db_path: &Path) -> bool {
    let instance_id = nestweaver_daemon::lifecycle::instance_id_from_db_path(db_path);
    let pidfile = nestweaver_daemon::lifecycle::pidfile_path(&instance_id);
    let was_running = pidfile.exists()
        && nestweaver_client::autostart::read_pid(&pidfile)
            .is_some_and(nestweaver_client::autostart::is_process_alive);
    if was_running {
        eprintln!("Stopping daemon to acquire write lock (will restart after)...");
        if let Some(pid) = nestweaver_client::autostart::read_pid(&pidfile) {
            unsafe { libc::kill(pid, libc::SIGTERM) };
            for _ in 0..50 {
                std::thread::sleep(std::time::Duration::from_millis(100));
                if !nestweaver_client::autostart::is_process_alive(pid) {
                    break;
                }
            }
        }
    }
    was_running
}

/// Restart the daemon after direct-mode work. Logs on failure.
fn restart_daemon(db_path: &Path, config: Option<&Path>) {
    eprintln!("Restarting daemon...");
    if let Err(e) = nestweaver_client::autostart::ensure_daemon(db_path, config) {
        eprintln!("Warning: failed to restart daemon: {e}");
    }
}
```

- [ ] **Step 2: Replace all three inline blocks with helper calls**

Index path:
```rust
let daemon_was_running = stop_daemon_if_running(&db_path);
// ... indexing ...
if daemon_was_running {
    restart_daemon(&db_path, config.as_deref().map(std::path::Path::new));
}
```

Brain add path:
```rust
let daemon_was_running = stop_daemon_if_running(&db_path);
// ... indexing ...
if daemon_was_running {
    restart_daemon(&db_path, None);
}
```

Materialize-projects path:
```rust
let daemon_was_running = stop_daemon_if_running(&db_path);
// ... materialize ...
if daemon_was_running {
    restart_daemon(&db_path, Some(config.as_path()));
}
```

Remove the now-unused `daemon_instance_id`, `daemon_pid_path`, `daemon_was_running_brain` variables.

- [ ] **Step 3: Run clippy + fmt + full test**

Run: `cd /Users/korykehl/dev/workspace/nestweaver && cargo clippy -p nestweaver -- -D warnings && cargo fmt --all && cargo test -p nestweaver`

- [ ] **Step 4: Commit**

```bash
git add src/main.rs
git commit -m "refactor(cli): extract stop_daemon_if_running / restart_daemon helpers

Deduplicates the daemon stop-restart pattern used by index, brain add,
and materialize-projects."
```

---

### Task 5: Log Cache Serialization Failures

**Files:**
- Modify: `crates/nestweaver-mcp/src/tools.rs:449-451`

When `serde_json::to_vec(&result)` fails in the cache-miss path, the error is silently swallowed. The result is still returned, but the entry is never cached with no diagnostic.

- [ ] **Step 1: Add tracing::debug on serialization failure**

```rust
// Before (line ~449-451):
if let Ok(bytes) = serde_json::to_vec(&result) {

// After:
match serde_json::to_vec(&result) {
    Ok(bytes) => {
        // ... existing insert + flush logic ...
    }
    Err(e) => {
        tracing::debug!(tool = name, "cache: serialization failed, skipping cache insert: {e}");
    }
}
```

Restructure the surrounding code to use `match` instead of `if let Ok`. Move the `RESPONSE_CACHE.with` block and flush counter logic into the `Ok` arm.

- [ ] **Step 2: Run clippy + fmt + commit**

```bash
git add crates/nestweaver-mcp/src/tools.rs
git commit -m "fix(mcp): log cache serialization failures instead of silently skipping"
```

---

### Task 6: Add Delete-Path Tests

**Files:**
- Modify: `crates/nestweaver-store/src/lib.rs` (test module)

The review noted no unit tests for `delete_vault_cascade` (bulk version) or `bulk_delete_repo_files_and_symbols`. Add focused tests.

- [ ] **Step 1: Write test for bulk delete_vault_cascade**

```rust
#[test]
fn test_delete_vault_cascade_bulk_removes_all_node_types() {
    let store = GraphStore::in_memory().unwrap();
    // 1. Insert vault, 3 notes, headings, sections, tags, tag edges, wikilink edges
    // 2. Assert counts before delete
    // 3. Call delete_vault_cascade
    // 4. Assert all counts are 0
    // Verify: notes, headings, sections, tags, unresolved wikilinks all gone
}
```

Look at existing test patterns in `lib.rs` for how to insert notes, headings, sections, and tags. The test `bulk_vault_write_inserts_notes_headings_sections_and_edges` shows the insert side — mirror it for the delete side.

- [ ] **Step 2: Write test for bulk_delete_repo_files_and_symbols**

```rust
#[test]
fn test_bulk_delete_repo_files_and_symbols_removes_all() {
    let store = GraphStore::in_memory().unwrap();
    // 1. Insert repo, 3 files, 5 symbols, file-symbol edges
    // 2. Assert counts before delete
    // 3. Call bulk_delete_repo_files_and_symbols
    // 4. Assert file count and symbol count are 0
    // 5. Assert returned counts match what was inserted
}
```

- [ ] **Step 3: Run tests + clippy + fmt**

Run: `cd /Users/korykehl/dev/workspace/nestweaver && cargo test -p nestweaver-store && cargo clippy -p nestweaver-store -- -D warnings && cargo fmt --all`

- [ ] **Step 4: Commit**

```bash
git add crates/nestweaver-store/src/lib.rs
git commit -m "test(store): add unit tests for bulk cascade delete paths"
```
