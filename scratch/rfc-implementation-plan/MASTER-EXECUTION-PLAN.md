---
title: NestWeaver — Master Execution Plan (RFC v0.9.1+ feature track)
status: ready-to-execute
created: 2026-05-29
supersedes_for_execution: IMPLEMENTATION-PLAN.md, BUILD-SPEC-*.md (this is the conductor's score;
                          those are the parts — open the referenced BUILD-SPEC section for full
                          tasks/tests on any item)
verified_against: feat/ui-next-gen-r3f working tree, 2026-05-29
---

# NestWeaver — Master Execution Plan

One sequenced backlog to execute from. Each item is a PR-sized unit with: **status** (what's
already built), **verified surface**, **acceptance**, **deps/gate**, and a **spec ref** for full
task/test depth. Bugs #12 and #19 are already shipped on this branch.

**Status legend:** 🟢 greenfield · 🟡 partially built (finish/wire) · 🔵 reuse-heavy · ⛔ gated.

---

## 1. Read-first corrections (these bite if missed)

1. **`Symbol` has no `end_line`** — keystone schema add; blocks F5/F8/F3-symbol. (`nodes.rs:140`;
   parser discards available `end_position()` at `parse.rs:593`.)
2. **`graph_generation` is in-memory, reset to 0 each open, bumped only by watchers — never by
   `index`** (`db.rs:37/53/69/108`). Must be persisted + bumped on index (M0.3) before it can key a
   cache/bundle.
3. **F6 priors apply AFTER RRF** (`rrf_fuse` ~`:1001`) or they no-op; **F7 PRF weights are
   rank-only** through RRF (set expectations). **Crate correction (review):** `query.rs`
   (`rrf_fuse`, `BrainNode:661`, `render_brain_node:1060`, `build_brain_context_hybrid_with_aliases:763`,
   `expand_query_with_aliases:1271`) lives in **`nestweaver-engine`**, NOT `nestweaver-store` — the
   build specs and ADDENDUM mislabel the crate (line numbers are right). F6/F7/F8/F1 modify
   *engine*.
4. **F12 clamp bug:** `(1+w·(score−0.5))` with score∈[0,1] needs **w=1.2** to reach `[0.4,1.6]`, not
   0.6.
5. **F1 PPR-consumption is ALREADY BUILT** (5% additive teleport blend, `ranking.rs:599`); **F7
   alias expansion is ALREADY BUILT** (`expand_query_with_aliases`). Don't re-implement.
6. **F2 is greenfield**: the live `cross_repo_contracts` tool does name/import matching; the real
   contract matcher is unwired dead code. `detect_frameworks()` exists but is never called.
7. **F13 is reuse-heavy**: `traverse.rs::impact`, `process.rs::detect_changes_impact`,
   `entry_points.rs::is_test_file`.
8. **F9 Leiden runs on the code graph only** (`cluster_dispatch.rs`); doc-graph queries are greenfield.
9. **PPR is pure in `nestweaver-algorithms`** — apply rank multipliers (F12) at *consumption* in
   `ranking.rs`, not in the pure PPR.
10. **Step Zero is configuration + dogfooding** (MCP isn't configured → tool not in the loop); the
    **vault Project is now Journey 8** in the benchmark.
11. **Quality gains are corpus-dependent** — F7/F1/F12/F17 stay off-by-default and gated on the
    **P0.3 eval harness**; **measure our own token savings** for F5/F8 (vendor %s are unproven).

---

## 2. Feature status at a glance

| Feature | Status | Note |
|---------|--------|------|
| F3 regex_search / F4 count | 🟢 | builds trigram index; deps present |
| F5 read_symbols | 🟢→🔵 | blocked on `end_line`; `symbols_in_file` exists |
| F6 path priors | 🟢 | post-RRF multiply; `globset` available |
| F7 PRF + aliases | 🟡 | aliases done; **PRF pass new** |
| F8 inline bodies | 🟢 | ~15 LOC + F5 for symbol bodies |
| F9 doc-graph tools | 🟢→🔵 | Leiden exists (code-only); queries new |
| F1 feedback loop | 🟡 | **consumption + FollowUp + session_id done**; add TerminalSuccess + hardening |
| F2 contract linking | 🟢 | greenfield; wire `detect_frameworks` first |
| F12 CodeRank | 🟢→🔵 | git-history miner new; multiply at rank-read |
| F13 affected_tests | 🔵 | reuse impact/traverse + test heuristics |
| F10 investigate | 🟢 | composes F7/F8/F9 |
| F11 memory-bank | 🟢→🔵 | typed edges + checks; orphans/broken reuse F9 |
| F14/F15 guidance | 🟢 ⛔ | Tier E3; defense-in-depth, not enforcement |
| F16 cache | 🟢 ⛔ | adversarial: instrument→memo only; disk after P0.2+hit-rate |
| F17 reranker | 🟢 ⛔ | adversarial: LambdaMART experiment, not the net; only if F7+F1 leave a gap |

---

## 3. Execution sequence (dependency-ordered)

### Milestone 0 — Foundations
| ID | Item | Status | Acceptance | Deps | Spec |
|----|------|--------|-----------|------|------|
| **M0.1** | Configure MCP (`setup claude-code`) + dogfood; run benchmark incl. **J8** | — | J8 runs; baseline timings captured | — | ADDENDUM §A |
| **M0.2** | **P0.1 `Symbol.end_line`** (keystone) | 🟢 | `symbol --json` has `end_line`; reindex populates | — | wave1 P0.1 |
| **M0.3** | P0.2 persist + bump `graph_generation` | 🟡 | survives reopen; bumps on `index` | — | wave1 P0.2 |
| **M0.4** | P0.3 eval harness (nDCG@10/MRR, time-CV) | 🟢 | reproducible baseline number | — | wave1 P0.3 |
| **M0.5** | P0.5 success-signal labeler (`TerminalSuccess` + neg signal) | 🟡 | new kinds recorded; sidecar bounded | — | wave1 P0.5 / wave4 F1 |

### Milestone 1 — Token-cost + ranking quick wins
| ID | Item | Status | Deps | Spec |
|----|------|--------|------|------|
| **M1.1** | F5 read_symbols | 🟢 | M0.2 | wave1 F5 |
| **M1.2** | F8 inline bodies | 🟢 | M1.1 (symbol bodies) | wave1 F8 |
| **M1.3** | F3 regex_search (+ trigram index) | 🟢 | (M0.2 for sym-text) | wave1 F3 |
| **M1.4** | F4 count_patterns | 🟢 | M1.3 | wave1 F4 |
| **M1.5** | F6 path priors | 🟢 | — | wave1 F6 |
| **M1.6** | F9 doc-graph tools (5) | 🔵 | — | wave2 F9 |

### Milestone 2 — Differentiator + PR-review
| ID | Item | Status | Deps/Gate | Spec |
|----|------|--------|-----------|------|
| **M2.1** | F2.0 wire `detect_frameworks` | 🟢 | — | wave2 F2.0 |
| **M2.2** | F2.1 contract nodes (OpenAPI/proto/GraphQL) | 🟢 | — | wave2 F2.1 |
| **M2.3** | F2.2 same-repo IMPLEMENTS (Spring/NestJS) | 🟢 | M2.1, M2.2 | wave2 F2.2 |
| **M2.4** | F2.3 cross-repo CONSUMES (gRPC/operationId) | 🟢 ⛔ | **demand count > threshold** | wave2 F2.3 |
| **M2.5** | F2.4 drift diagnostics | 🟢 | M2.2, M2.3 | wave2 F2.4 |
| **M2.6** | F13 affected_tests | 🔵 | — (parallel) | wave3 F13 |
| **M2.7** | W3.0 git-history miner | 🟢 | — | wave3 W3.0 |
| **M2.8** | F12 CodeRank | 🔵 ⛔ | M2.7; **gate P0.3** | wave3 F12 |

### Milestone 3 — Quality cohort (all gated on M0.4 / P0.3)
| ID | Item | Status | Deps/Gate | Spec |
|----|------|--------|-----------|------|
| **M3.1** | F7 PRF pass | 🟡 ⛔ | gate P0.3 | wave4 F7 |
| **M3.2** | F1 finish (TerminalSuccess + neg signal + exploration floor + `interactions show --uid`) | 🟡 ⛔ | M0.5; gate P0.3 | wave4 F1 |

### Milestone 4 — Composites
| ID | Item | Status | Deps | Spec |
|----|------|--------|------|------|
| **M4.1** | F10 investigate | 🟢 | F7, F8, F9, M0.3 | wave5 F10 |
| **M4.2** | F11 memory-bank | 🔵 | F9 | wave5 F11 |

### Milestone 5 — Contingent (carry gates)
| ID | Item | Status | Posture | Spec |
|----|------|--------|---------|------|
| **M5.1** | F14/F15 guidance | 🟢 ⛔ | defense-in-depth; build if §2.6 earns keep | wave5 F14/F15 |
| **M5.2** | F16 cache | 🟢 ⛔ | instrument → process-local memo; disk only after M0.3 + measured hit-rate | wave5 F16 |
| **M5.3** | F17 reranker | 🟢 ⛔ | only if F7+F1 leave a measured reorder gap → LambdaMART experiment, not the candle net | wave5 F17 |

---

## 4. Dependency graph

```
M0.2 end_line ─┬─► M1.1 F5 ─► M1.2 F8
               └─► M1.3 F3(sym-text)
M1.3 F3 ─► M1.4 F4
M0.3 graph_generation ─────────────► M4.1 F10 ;  M5.2 F16(disk)
M0.4 eval harness ──gate──► M2.8 F12, M3.1 F7, M3.2 F1, M5.3 F17
M0.5 labeler ─► M3.2 F1
M2.7 git miner ─► M2.8 F12
F9 (M1.6) ─► M4.1 F10, M4.2 F11
F7+F8+F9 ─► M4.1 F10
F6/F9/F13/F2* — independent of the above chains (can parallelize)
```

## 5. Gates (explicit "ready when")

- **P0.3 eval harness must exist** before F7/F1/F12/F17 default on (their value is otherwise
  unmeasurable).
- **P0.2 (persisted generation)** before F10 bundle keying or any F16 disk cache.
- **F2.3** only after a **demand count** of real cross-repo gRPC/typed-client couplings clears a bar.
- **F12/F7/F1** must clear P0.3 (CI excludes zero) to default on; otherwise stay flag-gated.
- **F16** only after a **measured hit-rate** clears a pre-committed bar (else process-local memo only).
- **F17** only if a **durable reorder-specific gap** remains after F6/F7/F1, validated on P0.3.

## 6. First PR — start here: M0.2 / P0.1 `Symbol.end_line`

No deps, unblocks the most — but the field touches many construction sites (verified blast radius;
review-corrected from the original 4-step sketch). Full scope:

**A. Schema + parser**
1. Add `pub end_line: u32` to `RawSymbol` (`parser/src/parse.rs:59`) and `Symbol`
   (`schema/src/nodes.rs:140`).
2. Tree-sitter path: set `end_line = node.end_position().row as u32 + 1` next to `start_line`
   (`parse.rs:593`); wire into the RawSymbol literal at `parse.rs:654-664`.
3. **15 regex/line-based parsers** (astro, cobol, fortran, frameworks, groovy, hcl, julia, objc,
   pascal, powershell, sql, svelte, systemverilog, vue, zig) build `RawSymbol` with **no tree-sitter
   node** → set `end_line = start_line` (single-line). (~50 literals — the largest omission in the
   original sketch.)
4. Production `RawSymbol→Symbol` mapping: `index.rs:543`, `index.rs:1056`, `watch_code.rs:342`
   (and `cross_domain.rs:475/585`) → `end_line: raw.end_line`.

**B. Store**
5. Add `end_line INT64` to the Symbol DDL (`store/src/db.rs:231-244`) **and** an idempotent
   `ALTER TABLE Symbol ADD end_line INT64 DEFAULT 0` in `init_schema` (the DDL is `IF NOT EXISTS`,
   so existing DBs need the ALTER — mirror the pattern at `db.rs:207`).
6. Add `end_line` to **both** insert paths: `batch_insert_symbols_on` (`write.rs:145/151-154`, the
   *production* path) and `insert_symbol_with_conn` (`write.rs:87/94-97`).
7. Read: add `s.end_line` to `SYMBOL_COLUMNS` (`read.rs:181`) and read it in `row_to_symbol`
   (`read.rs:144`, column index 6 — shift the rest; update the doc-comment). `row_to_symbol` already
   `unwrap_or(0)`, so old DBs return `end_line: 0` without error.

**C. Compile blast radius (won't build otherwise)**
8. Update all remaining `Symbol`/`RawSymbol` struct literals (~37 `Symbol` + ~80 `RawSymbol` across
   schema/parser/resolver/store/engine/mcp/web, **incl. test/helper literals**). WASM crate is
   unaffected. "Existing tests green" depends on this.

**D. Migration reality**
9. `core_schema_hash` (`schema/src/version.rs`) is **computed, not a constant**, and `end_line` is
   already in `NODE_PROPERTIES` — adding it does **not** change the hash and does **not** force a
   local re-index (it only gates snapshot-pull). Old/unchanged files keep `end_line=0` until
   `index --force`. Document this; don't rely on a "hash bump."

**Tests (TDD):** multi-line fn → `end_line > start_line` matching source (tree-sitter); a
regex-language symbol → `end_line == start_line`; store round-trip insert→read equal; `symbol --json`
shows `end_line` (auto-serialized via serde on `Symbol`).

**Verify:** `cargo test`, `clippy --all-targets -- -D warnings`, `fmt --all --check`; `index --force`
a fixture; confirm `end_line` populated.

**Acceptance:** a fresh `index --force` populates correct `end_line` for tree-sitter symbols and
`start_line` for regex-language symbols; `symbol --json` includes `end_line`; all existing tests green.

---

## 7. Review findings (independent pre-execution review, applied)

Two review agents stress-tested this plan and verified the first PR against code. The milestone DAG
is **acyclic and correctly ordered**; M0→M1 startability, the gates (P0.3/P0.2/demand-count/hit-rate/
CI), F1-already-built, F12 clamp `w=1.2`, F2-greenfield, and `graph_generation` not-persisted all
**confirmed**. Issues found and how they're handled:

**Must-fix (applied):**
1. **Wrong crate for `query.rs`** — it's `nestweaver-engine`, not store. Affects F6/F7/F8/F1 refs in
   the build specs + ADDENDUM (line numbers correct). → Corrected in §1.3; **when executing those
   items, read `nestweaver-engine/src/query.rs`.**
2. **First PR (P0.1) was under-scoped** — missed the 15 regex parsers, the `batch_insert_symbols_on`
   production insert path, the `RawSymbol→Symbol` mapping sites, the ~117 struct-literal compile
   sites, and the migration mechanism (`core_schema_hash` does NOT force reindex). → §6 fully
   rewritten.
3. **`is_test_file` is not a reusable fn** — it's a local `let` + per-language inline checks in
   `entry_points.rs`. → **F13 (M2.6) gains an explicit task: extract a generalized test-file
   predicate first.** Re-budget F13 from "mostly reuse" to "reuse + one extraction."
4. **`TerminalSuccess` is double-owned by P0.5 and F1.** → Scope split: **P0.5/M0.5** = add the
   `EventType::TerminalSuccess` variant + recording plumbing; **F1/M3.2** = consumption tuning,
   anti-feedback hardening (exploration floor + negative signal), and `interactions show --uid`.

**Should-fix:**
5. F2.2 edge-insert is at `write.rs:376` (not `:385`); M0.3 must hook the real index entry points
   `index_directory_with_options` (`index.rs:209`) and `incremental_index` (`index.rs:802`).
6. Make explicit: **core F13 ships without W3.0** (co-change is an optional fallback, not a v1 dep) —
   so M2.6 is not blocked by M2.7.

These corrections override the corresponding lines in the wave build-specs. The plan is otherwise
execution-ready; **start with §6 (P0.1).**
