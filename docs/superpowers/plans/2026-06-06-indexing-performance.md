# Indexing Performance — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Reduce indexing time for large monoliths (20k+ files, 200k+ symbols) from ~45 minutes to under 10 minutes by parallelizing the sequential bottlenecks and eliminating redundant work.

**Architecture:** Five tasks targeting three bottlenecks: reference resolution (sequential → parallel), enclosing-symbol lookup (O(n) → O(log n)), and redundant file I/O (eliminate disk re-reads + batch DB deletes). Each task is independently shippable.

**Tech Stack:** Rust (edition 2024), rayon 1.10, tree-sitter 0.26, LadybugDB (lbug 0.16.1).

**Conventions:** Conventional commits (`perf:` scope), `cargo clippy --workspace --all-targets -- -D warnings`, `cargo fmt --all`, no `unwrap()`/`expect()` outside tests, no AI attribution.

**Research backing:** Each optimization is validated by external evidence:
- Parallel resolution: ECOOP 2021 "Scope States" paper (5x on 8 cores), TypeScript 7.0 parallel type checking, pyright `--threads` (3x+), NPB-Rust rayon benchmarks (12x on 40 threads for lookup-heavy workloads), rustc parallel front-end (42% wall-time reduction).
- Binary search: rowan (rust-analyzer's syntax tree) uses `binary_search_by` for `child_at_range`. Crossover vs linear at ~100-150 elements; NestWeaver files average ~200 symbols. 26x fewer comparisons per file.
- Source retention: rust-analyzer VFS keeps all source in memory by design. clangd holds full AST (including source) during index build. Win is modest on warm local SSD (~1.6s) but massive on NFS (~13 minutes for 20k files).
- Batch deletes: Kuzu Transactions V2 issue documents per-commit WAL flush as fundamental bottleneck. Neo4j recommends 10k-row batch deletes. `bulk_delete_repo_files_and_symbols` already exists.
- Parallel type env: rustc parallelizes per-crate type checking with rayon. Kotlin K2 achieves 156-194% faster analysis. Mypy 2.0 `--threads` gives up to 5x.

---

### Task 1: Parallelize Reference Resolution

**Files:**
- Modify: `crates/nestweaver-resolver/src/resolve.rs:49-518`

This is the highest-impact change. `resolve_references_with_context` processes all files sequentially. Each file's references are resolved independently against the shared read-only `symbol_map` and `import_graph`. The outer `for (file_path, symbols, references) in files` loop (line 98) is embarrassingly parallel.

- [ ] **Step 1: Add rayon dependency to nestweaver-resolver**

Check `crates/nestweaver-resolver/Cargo.toml` — if `rayon` is not already a dependency, add it. It's already a workspace dependency (used by nestweaver-engine), so use `rayon = { workspace = true }`.

- [ ] **Step 2: Write a determinism test**

Before changing the implementation, add a test that verifies reference resolution produces identical edges regardless of execution order. In `crates/nestweaver-resolver/src/resolve.rs` test module:

```rust
#[test]
fn parallel_resolution_matches_sequential() {
    // Build a multi-file fixture with cross-file references
    // Run resolve_references_with_context
    // Collect edges, sort by (source_uid, target_uid, edge_type)
    // Run again, sort, assert identical
}
```

Use an existing test fixture or build one with 10+ files and 50+ cross-file references. The key assertion: sorted edge output is byte-identical across runs.

- [ ] **Step 3: Run the test to verify it passes (baseline)**

Run: `cd /Users/korykehl/dev/workspace/nestweaver && cargo test -p nestweaver-resolver parallel_resolution`

- [ ] **Step 4: Convert the per-file loop to rayon par_iter**

In `resolve_references_with_context` (line 98), replace:

```rust
// Before (sequential):
for (file_path, symbols, references) in files {
    'ref_loop: for reference in references {
        // ... resolve each reference ...
        edges.push(edge);
    }
}

// After (parallel):
use rayon::prelude::*;

let file_edges: Vec<Vec<ResolvedEdge>> = files
    .par_iter()
    .map(|(file_path, symbols, references)| {
        let mut local_edges = Vec::new();
        'ref_loop: for reference in references {
            // ... exact same resolution logic ...
            // Push to local_edges instead of shared edges
            local_edges.push(edge);
        }
        local_edges
    })
    .collect();

let mut edges: Vec<ResolvedEdge> = file_edges.into_iter().flatten().collect();
```

The `symbol_map`, `extends_map`, and `graph` are all `&HashMap` / `&ImportGraph` — shared immutable references that are `Sync` and safe to access from rayon threads.

**Important:** The dedup set at the end (if one exists) must operate on the collected edges after the parallel phase, not during. Check if there's a `HashSet` or dedup operation inside the current loop body that uses shared mutable state — if so, move it to a post-collection pass.

Also check: the import-edge generation loop (around line 433: `for (file_path, _symbols, _references) in files`) is a separate loop that may also benefit from parallelization, but start with just the reference resolution loop.

- [ ] **Step 5: Run the determinism test again**

Run: `cd /Users/korykehl/dev/workspace/nestweaver && cargo test -p nestweaver-resolver parallel_resolution`

Expected: PASS — identical edges regardless of parallel execution order.

- [ ] **Step 6: Run full test suite + clippy**

Run: `cd /Users/korykehl/dev/workspace/nestweaver && cargo test --workspace && cargo clippy --workspace --all-targets -- -D warnings && cargo fmt --all`

- [ ] **Step 7: Commit**

```bash
git commit -m "perf(resolver): parallelize per-file reference resolution with rayon

The outer per-file loop in resolve_references_with_context is
embarrassingly parallel — each file's references are resolved
independently against shared immutable symbol_map and import_graph.

Research: ECOOP 2021 'Scope States' paper validates this pattern (5x on
8 cores). TypeScript 7.0, pyright, and rustc all parallelize equivalent
phases. NPB-Rust rayon benchmarks show 12x on 40 threads for
lookup-heavy workloads."
```

---

### Task 2: Replace find_enclosing_symbol O(n) with Binary Search

**Files:**
- Modify: `crates/nestweaver-resolver/src/resolve.rs:522-527`

`find_enclosing_symbol` does a linear scan of all symbols per reference. For a 5000-line Java file with 200 symbols and 1000 references, that's 200k comparisons. Binary search reduces to 7.6k (26x fewer).

- [ ] **Step 1: Write a test for binary search correctness**

Add to the test module in `resolve.rs`:

```rust
#[test]
fn find_enclosing_symbol_binary_search_matches_linear() {
    // Build a symbols array with 200 symbols at various start_lines
    // For 50 reference lines spread across the file,
    // verify binary search gives the same result as the current linear scan
}
```

- [ ] **Step 2: Add a debug assertion for sort order**

At the top of `find_enclosing_symbol`, add:

```rust
debug_assert!(
    symbols.windows(2).all(|w| w[0].start_line <= w[1].start_line),
    "find_enclosing_symbol requires symbols sorted by start_line"
);
```

This catches any future regression where symbols arrive out of order. The assertion is stripped in release builds (zero runtime cost).

- [ ] **Step 3: Replace the linear scan with partition_point**

```rust
fn find_enclosing_symbol(symbols: &[RawSymbol], ref_line: u32) -> Option<&RawSymbol> {
    debug_assert!(
        symbols.windows(2).all(|w| w[0].start_line <= w[1].start_line),
        "find_enclosing_symbol requires symbols sorted by start_line"
    );
    if symbols.is_empty() {
        return None;
    }
    let idx = symbols.partition_point(|s| s.start_line <= ref_line);
    if idx == 0 {
        None
    } else {
        Some(&symbols[idx - 1])
    }
}
```

`partition_point` returns the index where `start_line > ref_line` would be inserted. The symbol at `idx - 1` is the last one with `start_line <= ref_line` — exactly what the current linear scan returns.

- [ ] **Step 4: Run tests**

Run: `cd /Users/korykehl/dev/workspace/nestweaver && cargo test -p nestweaver-resolver && cargo test --workspace`

- [ ] **Step 5: Commit**

```bash
git commit -m "perf(resolver): use binary search for find_enclosing_symbol (O(n) → O(log n))

partition_point replaces the linear scan. For a file with 200 symbols
and 1000 references, this reduces comparisons from 200k to 7.6k (26x).

Tree-sitter emits symbols in source order (structural guarantee from the
LR parser). rowan (rust-analyzer) uses the same binary_search_by pattern
for child_at_range. Debug assertion added to catch ordering violations."
```

---

### Task 3: Retain Parsed Source to Avoid Disk Re-Reads

**Files:**
- Modify: `crates/nestweaver-engine/src/index.rs:452-470, 516-518, 820-880`

Phase 2 reads every file from disk (parallel parse), but only retains source for TS/JS. Phase 3 re-reads every file from disk (sequential type env build). This is 20k redundant `read_to_string` calls.

- [ ] **Step 1: Expand source retention in ParseOutcome**

In `index.rs`, the `ParseOutcome::Parsed` variant has `source: Option<String>` (line 468). Currently only retained for TS/JS (lines 516-518):

```rust
// Before:
let retained_source =
    matches!(*lang, Language::TypeScript | Language::JavaScript)
        .then(|| source.clone());

// After: retain for all languages that have type queries, with a size cap
const SOURCE_RETENTION_CAP: usize = 2 * 1024 * 1024; // 2 MB
let retained_source = if source.len() <= SOURCE_RETENTION_CAP {
    Some(source)
} else {
    None
};
```

Note: change `source.clone()` to just `source` (move, not clone) — the source `String` is not used after this point in the parallel closure, so moving avoids a heap allocation.

- [ ] **Step 2: Use retained source in type environment build**

In the type env build loop (lines 820-847), replace the disk re-read with the retained source:

```rust
// Before (line 825):
if let Ok(source) = std::fs::read_to_string(&full_path) {

// After:
// Build a lookup from the sequential collection phase
// (add source to parsed_files_for_resolver, or build a separate HashMap<String, String>)
```

You'll need to thread the source through. Options:
- **Option A:** Change `parsed_files_for_resolver` from `Vec<(String, Vec<RawSymbol>, Vec<RawReference>)>` to include the source: `Vec<(String, Vec<RawSymbol>, Vec<RawReference>, Option<String>)>`. Then the type env build loop uses the retained source if available, falls back to disk read if not (for files above the 2 MB cap).
- **Option B:** Build a separate `HashMap<String, String>` mapping `rel_path → source` during the sequential collection phase.

Option A is cleaner — follow it unless the tuple becomes unwieldy.

- [ ] **Step 3: Do the same for the cross-file return type propagation**

Lines 870-871 also re-read from disk:
```rust
if let Ok(source) = std::fs::read_to_string(&full_path) {
```

Use the same retained source here.

- [ ] **Step 4: Run full test suite**

Run: `cd /Users/korykehl/dev/workspace/nestweaver && cargo test --workspace`

- [ ] **Step 5: Commit**

```bash
git commit -m "perf(engine): retain parsed source to avoid redundant disk re-reads

Source strings are now moved (not cloned) from the parallel parse phase
into the sequential collection, eliminating 20k read_to_string calls
during type environment construction. Files over 2 MB fall back to disk.

rust-analyzer's VFS keeps all source in memory by design. Win is modest
on warm local SSD but eliminates ~13 minutes of I/O on NFS (20k files
× 40ms NFS RTT)."
```

---

### Task 4: Use Bulk Delete for Force Re-Index Cleanup

**Files:**
- Modify: `crates/nestweaver-engine/src/index.rs:709-724`

When re-indexing over an existing store, the current code loops over every changed file calling `delete_symbols_in_file` + `delete_file_node` (2-3 queries each). For a force re-index of 20k files, that's 60k queries. The `bulk_delete_repo_files_and_symbols` function (added in Phase 1 of the perf audit) does this in ~4 queries.

- [ ] **Step 1: Detect force re-index and use bulk delete**

In `index.rs` around line 712, the code checks `if existing_repo.is_some()`. For a force re-index, ALL files are in `all_files`. Replace the per-file loop with the bulk delete:

```rust
// Before (lines 712-724):
if existing_repo.is_some() {
    for file in &all_files {
        let _ = store.delete_symbols_in_file(&r_uid, &file.path);
        let _ = store.delete_file_node(&file.uid);
    }
    let _ = store.clear_repo_derived_nodes(&r_uid);
}

// After:
if existing_repo.is_some() {
    // For force re-index (all files present), use bulk delete
    if all_files.len() == files_count && files_unchanged == 0 {
        let _ = store.bulk_delete_repo_files_and_symbols(&r_uid);
    } else {
        // Incremental: only delete changed files
        for file in &all_files {
            let _ = store.delete_symbols_in_file(&r_uid, &file.path);
            let _ = store.delete_file_node(&file.uid);
        }
    }
    let _ = store.clear_repo_derived_nodes(&r_uid);
}
```

Actually, the condition for "force re-index" may not be detectable here since `files_unchanged` tracks filemeta-cache hits. Check how the `force` flag flows into this function. It may be simpler to check if `filemeta_cache.is_none()` (which is true for force re-index since the caller skips loading the cache).

Alternatively: if `all_files.len()` matches the total file count (no files were skipped by tiered change detection), use bulk delete.

- [ ] **Step 2: Run tests**

Run: `cd /Users/korykehl/dev/workspace/nestweaver && cargo test --workspace`

- [ ] **Step 3: Commit**

```bash
git commit -m "perf(engine): use bulk delete for force re-index cleanup

Replaces per-file delete loop (60k queries for 20k files) with a single
bulk_delete_repo_files_and_symbols call (~4 queries). Kuzu's WAL flushes
per commit make individual deletes catastrophically slow. Neo4j docs
explicitly recommend batch deletes of 10k+ rows."
```

---

### Task 5: Parallelize Type Environment Construction

**Files:**
- Modify: `crates/nestweaver-engine/src/index.rs:818-847`

Each file's `TypeEnvironment::build` is independent — reads only its own AST and symbols, produces a self-contained binding map. The sequential loop can use `rayon::par_iter`.

- [ ] **Step 1: Convert the type env build loop to par_iter**

```rust
// Before (lines 818-847):
let mut type_envs: HashMap<String, TypeEnvironment> = {
    let mut envs = HashMap::new();
    for (file_path, symbols, _references) in &parsed_files_for_resolver {
        let full_path = repo_path.join(file_path);
        if let Ok(source) = std::fs::read_to_string(&full_path) {
            // ... build env ...
            if env.binding_count() > 0 {
                envs.insert(file_path.clone(), env);
            }
        }
    }
    envs
};

// After:
use rayon::prelude::*;

let type_envs: HashMap<String, TypeEnvironment> = parsed_files_for_resolver
    .par_iter()
    .filter_map(|(file_path, symbols, _references, source_opt)| {
        let source = source_opt.as_deref().or_else(|| {
            // Fallback to disk read for files not retained (>2MB)
            std::fs::read_to_string(repo_path.join(file_path)).ok()
        }.as_deref())?;
        // ... handle the source lifetime issue — you may need to read into a local String ...
        let env = TypeEnvironment::build(source, language, symbols, file_ast_bindings);
        if env.binding_count() > 0 {
            Some((file_path.clone(), env))
        } else {
            None
        }
    })
    .collect();
```

Note: The borrow checker may complain about lifetimes when mixing `source_opt.as_deref()` with a disk-read fallback. You may need to use a local `String` variable:

```rust
.filter_map(|(file_path, symbols, _refs, source_opt)| {
    let source_string;
    let source: &str = if let Some(s) = source_opt.as_deref() {
        s
    } else {
        source_string = std::fs::read_to_string(repo_path.join(file_path)).ok()?;
        &source_string
    };
    // ... build env ...
})
```

- [ ] **Step 2: Keep cross-file propagation sequential**

The `seed_return_types` loop (lines 850-882) depends on ALL type envs being complete. It must remain sequential, AFTER the `par_iter().collect()` finishes. This is already the case since `collect()` is a barrier.

However, `seed_return_types` mutates individual type environments (`env.seed_return_types(...)` on line 873), so the `type_envs` HashMap must be `mut`. Declare it as `let mut type_envs: HashMap<...> = ...par_iter()...collect();`.

- [ ] **Step 3: Run full test suite**

Run: `cd /Users/korykehl/dev/workspace/nestweaver && cargo test --workspace`

- [ ] **Step 4: Commit**

```bash
git commit -m "perf(engine): parallelize type environment construction with rayon

Each file's TypeEnvironment::build is independent — reads only its own
AST and symbols. Cross-file return type propagation remains sequential
(depends on all envs being complete).

rustc parallelizes per-crate type checking with rayon. Kotlin K2 achieves
156-194% faster analysis. Mypy 2.0 --threads gives up to 5x."
```

---

## Expected Impact

For a 20k-file Java monolith currently taking ~45 minutes:

| Phase | Before | After | Improvement | Research basis |
|-------|--------|-------|-------------|---------------|
| Reference resolution | ~15-25 min (sequential) | ~3-6 min (par_iter, 8 cores) | **3-6x** | ECOOP 2021: 5x; pyright: 3x+ |
| find_enclosing_symbol | O(n) per ref, ~200k comparisons/file | O(log n), ~7.6k comparisons/file | **26x fewer comparisons** | rowan binary_search_by pattern |
| Type env build | ~5-10 min (sequential + disk re-reads) | ~1-3 min (parallel + in-memory) | **3-5x** | rustc: 42% reduction; Kotlin K2: 156-194% |
| Re-index cleanup | ~5-10 min (60k queries) | ~seconds (4 queries) | **order of magnitude** | Kuzu WAL flush per commit; Neo4j batch docs |

**Conservative total estimate: 45 min → 8-15 min (3-5x overall)**
**Optimistic estimate: 45 min → 5-8 min (6-9x overall)**

The actual improvement depends on the user's hardware (core count, disk type) and repo characteristics (file size distribution, reference density). The `--stats` flag should be enhanced to report per-phase timing so users can see where their time goes.
