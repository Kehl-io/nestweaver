---
title: Build-ready spec — Phase 0 enablers + Wave 1 features
created: 2026-05-29
companion_to: IMPLEMENTATION-PLAN.md, ADDENDUM-evidence-and-findings.md
scope: The executable front of the plan. Surface verified against real code (ADDENDUM §B).
       Later waves stay at strategic depth in IMPLEMENTATION-PLAN.md until reached.
note: No build/defer/kill calls — every RFC feature is still in scope; this specifies the ones built first.
---

# Build Spec — Phase 0 (enablers) + Wave 1 features

Each item: **Goal · Verified surface · Schema/signatures · Tasks (ordered) · Tests (TDD) ·
Acceptance · Effort · Deps**. Effort: S ≈ ≤1 day, M ≈ 2–4 days, L ≈ 1–2 weeks.

Wave 1 is chosen as the **surface-verified, dependency-clean starting chain**:
`P0.1 end_line → F5 → F8(sym)` and `P0.4 trigram → F3 → F4`, plus `F6` (standalone).

---

## Phase 0 — Enablers

### P0.1 — `Symbol.end_line`
- **Goal.** Every Symbol carries its end line so symbol-window reads (F5), inline bodies (F8), and
  symbol-text regex (F3) can slice source precisely.
- **Verified surface.** `Symbol` (`crates/nestweaver-schema/src/nodes.rs:140`, add field);
  `RawSymbol` (`crates/nestweaver-parser/src/parse.rs:~62`); parser captures only
  `node.start_position()` at `parse.rs:~593` — `node.end_position().row` is available and discarded;
  Symbol-node DDL + `insert_symbol_with_conn` (`crates/nestweaver-store/src/write.rs:87`); Symbol
  row read (`read.rs`); schema version (`crates/nestweaver-schema/src/version.rs`).
- **Schema/signatures.** Add `pub end_line: u32` to `RawSymbol` and `Symbol`. Add `end_line INT`
  to the Symbol table DDL. Bump `core_schema_hash` → forces re-index.
- **Tasks.** (1) add field to `RawSymbol`+`Symbol`; (2) in parser set `end_line =
  node.end_position().row as u32 + 1`; (3) DDL column + write param + read extraction; (4) bump
  schema version; (5) graceful default for pre-migration rows (`end_line = start_line` if NULL).
- **Tests (TDD).** Parser unit test: a known multi-line function has `end_line > start_line` and
  matches the source. Store round-trip: insert Symbol with `end_line`, read it back equal.
- **Acceptance.** `nestweaver symbol <name> --json` includes `end_line`; a fresh index populates it
  for all symbols.
- **Effort.** M. **Deps.** none. **Blocks.** F5, F8(symbol path), F3(symbol-text path).

### P0.2 — Persist + bump `graph_generation`
- **Goal.** Make the generation counter a usable cross-process key (today it's in-memory, reset to
  0 each open, and bumped only by the watcher loops — ADDENDUM §B). Enabler for any disk cache
  (F16) and F10 bundle TTL keying. *Building this does not commit to F16.*
- **Verified surface.** `crates/nestweaver-store/src/db.rs:37/53/69/108` (constructors set
  `AtomicU64::new(0)`), `:157-166` (read/bump); bump sites `watcher.rs:341`, `watch_code.rs:271`;
  one-shot index path `crates/nestweaver-engine/src/index.rs`.
- **Schema/signatures.** Persist generation to a sidecar (`<db>.generation`) or a meta row; load on
  open; `pub fn graph_generation(&self) -> u64` already readable.
- **Tasks.** (1) persist on every bump; (2) load persisted value on open instead of 0; (3) bump at
  the end of `index`/`incremental_index`, not just the watcher; (4) expose for cache keys.
- **Tests.** open → index → reopen observes an incremented, persisted value; two concurrent opens
  read the same persisted generation.
- **Acceptance.** Generation survives reopen and increments on a one-shot `index`.
- **Effort.** S. **Deps.** none. **Blocks.** F16 (if pursued), F10 bundle keying.

### P0.3 — Retrieval-quality eval harness (§2.7)
- **Goal.** nDCG@10 / MRR / precision@k over NestWeaver's *own* code+notes corpus. Gates every
  Tier-E2 (ranking-quality) feature — those gains are invisible to the speed/coverage journeys
  benchmark (ADDENDUM §A.3).
- **Verified surface.** `benches/brain_benchmarks.rs` is criterion **speed only**; no quality
  harness exists. New: `benches/` or `scratch/` runner + a judged query set.
- **Schema/signatures.** A judged-query file: `{query, intent, graded_relevance: {uid: 0..3}}`.
  Runner emits per-query nDCG@10/MRR/p@k + aggregate with **time/query-based CV** and per-query
  win/loss + CI.
- **Tasks.** (1) author ~30–50 judged queries spanning code + the new J8 vault path; (2) runner
  scoring the current hybrid ranker; (3) snapshot a **baseline** to beat; (4) wire as the gate for
  F6/F7/F1/F12/F17.
- **Tests.** Deterministic nDCG on a tiny fixture with known ideal order.
- **Acceptance.** Produces a reproducible baseline number; re-runnable pre/post a ranking change.
- **Effort.** M (label authoring is the judgment-heavy part). **Deps.** none. **Gates.**
  F6, F7, F1, F12, F17.

### P0.4 — Trigram index (shared substrate for F3/F4)
- Specified inline under **F3** (F3 builds it; F4 reuses it).

### P0.5 — Interaction success-signal labeler (§2.4)
- **Goal.** Label interaction events so F1 (and later F17) have a safe success signal. Most of the
  schema already exists.
- **Verified surface.** `crates/nestweaver-engine/src/interactions.rs` — `InteractionEvent`
  **already has `session_id`**; `EventType` = Query/Access/FollowUp/Impact. Recording in
  `crates/nestweaver-mcp/src/lib.rs:314` `record_interaction`. `load_interaction_scores`
  (`interactions.rs:456`) returns `Option<HashMap<String,f64>>`, loaded at MCP startup but
  **consumed nowhere** (the F1 gap).
- **Schema/signatures.** Add `EventType::TerminalSuccess`; success heuristic = surfaced UID then
  *edited/written* (positive) | next action is another search/reformulation (negative) | bare
  access (weak). Decay `R = e^(−Δt/S)` (MemoryBank).
- **Tasks.** (1) add the event kind; (2) record success/negative transitions in the MCP layer;
  (3) prune at `R<ε`. (Consumption into ranking is F1, not here.)
- **Tests.** A scripted session (surface→edit) yields a positive label; surface→re-search yields
  negative; scores decay over time.
- **Acceptance.** `interactions show` reflects the new kinds; sidecar stays bounded.
- **Effort.** S–M. **Deps.** none. **Feeds.** F1, F17.

> P0.6 guidance store (F14/F15 prereq) is deferred to its wave — those are Tier-E3 / agent-harness.

---

## Wave 1 — features

### F5 — `read_symbols`
- **Goal.** Return a symbol's source span (± neighbors, optional comment stripping), token-budgeted.
  Primary Bash/`Read`-displacement lever (ADDENDUM §A.1).
- **Verified surface.** `symbols_in_file` (`read.rs:580`), `lookup_symbols_by_name` (`read.rs:299`,
  exact, returns `Vec<Symbol>`); parser language split = 17 tree-sitter / ~8 regex
  (`parse.rs:502-517`); **comment nodes are NOT currently extracted** — stripping needs new
  tree-sitter comment queries.
- **Schema/signatures.** MCP `read_symbols(uids_or_fqns: [String], strip_comments?: bool,
  include_neighbors?: u8, token_budget?: usize)` → `{uid, path, start_line, end_line, body, kind,
  comments_stripped, truncated?, dropped?}`. CLI `nestweaver read-symbols <uid|fqn>… [--strip-comments]
  [--neighbors N] [--token-budget N]`.
- **Tasks (ordered).** (1) resolve FQN/uid → Symbol(s) (ambiguous → `disambiguate: true`); (2) read
  span `start_line..end_line` from disk; (3) optional neighbors via `symbols_in_file` ordered by
  line; (4) optional comment strip — tree-sitter comment query per grammar language, conservative
  line-prefix for regex languages, **default OFF**; (5) token-budget truncation in input order with
  dropped list.
- **Tests (TDD).** Single symbol body equals source slice; `--strip-comments` reduces line count on
  a commented fn but not code lines; ambiguous FQN returns all + flag; budget truncation drops the
  right ones.
- **Acceptance.** RFC F5 acceptance tests pass; stripped body line-count < raw.
- **Effort.** M. **Deps.** P0.1 (`end_line`).

### F8 — Tiered display (inline body for high-confidence hits)
- **Goal.** Inline body when normalized relevance ≥ threshold, saving a round-trip.
- **Verified surface.** `BrainNode{uid,kind,title,location,relevance}` (`query.rs:661`); populate in
  `render_brain_node` (`query.rs:1060`); serialize in `crates/nestweaver-mcp/src/tools.rs` +
  CLI `print_brain_context_json`. Note/Section bodies already in store; Symbol bodies via F5.
- **Schema/signatures.** Add `pub inline_body: Option<String>` to `BrainNode`. Config
  `[response] inline_body_threshold = 0.75`, `inline_max_body_tokens = 800`. Opt-in:
  `--inline-bodies` / `include_bodies: true`.
- **Tasks.** (1) add field; (2) populate when `relevance ≥ threshold` & opt-in (Note/Section text
  from store; Symbol via F5 path, comment-stripped); (3) **normalize relevance + gap-check** before
  thresholding; (4) count inline bodies against `token_budget` ahead of metadata, downgrade
  lower-ranked to metadata-only if budget would blow.
- **Tests (TDD).** Top hit ≥ threshold has `inline_body`; below-threshold doesn't; tight budget
  yields 1–3 inline, rest metadata; off by default.
- **Acceptance.** RFC F8 acceptance tests pass.
- **Effort.** S–M. **Deps.** F5 (Symbol bodies); Note/Section free.

### F3 — `regex_search` (+ builds the trigram index)
- **Goal.** First-party regex over symbol/section/note text with a trigram pre-filter; keeps skills
  off raw `rg`/`grep` (ADDENDUM §A.1).
- **Verified surface.** Greenfield in `crates/nestweaver-store`; `regex` + `regex-syntax` available
  (`Cargo.lock`); Tantivy indexes notes/sections already.
- **Schema/signatures.** New table `trigram_postings { trigram: u32, node_uid: String }`
  (doc-ID-only — Cox/codesearch style, smallest + incrementally updatable). `--with-trigrams`
  opt-in flag on `index`. New `crates/nestweaver-store/src/regex.rs`. MCP
  `regex_search(pattern, path_prefix?, kinds?, limit?, max_millis?)`; CLI parity. Response mirrors
  `brain_search`.
- **Tasks (ordered).** (1) trigram extraction (lowercased 3-grams) at index time → postings table;
  (2) query planning via `regex_syntax::hir::literal::Extractor` (`Seq::is_finite()`, exact vs
  inexact) → required-trigram AND/OR set; (3) intersect postings → candidate UIDs → fetch text →
  run full `regex`; (4) fallback to scan when `Seq` infinite; (5) budget: candidate cap 5000 +
  timeout 2s (`--max-millis`), return `truncated: true`.
- **Tests (TDD).** Literal pattern returns expected hits; no-literal pattern (`.{4,}`) falls back
  and returns `truncated` under timeout; trigram prefilter rejects known non-matches (assert
  candidate set < corpus).
- **Acceptance.** RFC F3 acceptance tests; index-size overhead measured on the live DB before
  defaulting on.
- **Effort.** M–L. **Deps.** none (Symbol-text path wants P0.1; note/section text is ready).

### F4 — `count_patterns`
- **Goal.** Counts-only companion (files matched + total occurrences + per-file top), multi-pattern.
- **Verified surface.** Reuses F3's trigram prefilter; `regex` count mode.
- **Schema/signatures.** MCP `count_patterns(patterns: [String], path_prefix?, kinds?)` →
  per-pattern `{pattern, total_matches, files_matched, top_files:[{path,count}]}`. CLI
  `nestweaver count-patterns 'A' 'B' [--path-prefix …]`.
- **Tasks.** (1) union candidate sets across patterns; (2) scan each candidate doc once, attribute
  counts per pattern; (3) no result materialization.
- **Tests (TDD).** Counts match `rg -c ± 0` on a fixture; stable across runs; multi-pattern in one
  call.
- **Acceptance.** RFC F4 acceptance tests.
- **Effort.** S. **Deps.** F3.

### F6 — Per-path `dampen`/`boost` ranking priors
- **Goal.** Continuous path-glob prior on relevance; cheap, standalone ranking-policy knob.
- **Verified surface.** Apply **post-fusion**: after `query.rs::rrf_fuse` (~`query.rs:966`) and
  before the seed/connected split (`query.rs:972-981`) where fused scores become `BrainNode.relevance`.
  `globset` available (transitive via `ignore`).
- **Schema/signatures.** `[ranking]` in `InstanceConfig` / per-repo `.nestweaver.toml`:
  `dampen=[{glob,multiplier}]`, `boost=[…]`. Resolve path→prior once at open into a sidecar. CLI
  `nestweaver ranking explain <uid>` → `{base, matched_rules, final}`.
- **Tasks.** (1) parse `[ranking]` + compile globs; (2) at the post-fusion point multiply
  `node.relevance` by the matched multiplier (last-match-wins), clamp the **product** to
  `[0.05, 5.0]`; (3) `ranking explain` dry-run; (4) record matched rule when `--debug`.
- **Tests (TDD).** A dampened path drops below an undampened peer; clamp bounds a stacked product;
  `ranking explain` shows base ≠ final with the matched rule.
- **Acceptance.** RFC F6 acceptance tests; dampen ≠ exclude (floor 0.05).
- **Effort.** S–M. **Deps.** none.

---

## Wave 1 dependency order (build sequence)

```
P0.1 end_line ─┬─► F5 ─► F8(symbol)            P0.4/F3 trigram ─► F4
               └─► F3 symbol-text path          F6 (independent, any time)
P0.2 generation, P0.3 eval-harness, P0.5 labeler ── run in parallel; gate later waves
```

**Next waves (specify when reached):** F9 (doc-graph), F2-core (gRPC + spec nodes + same-repo
handlers, per ADDENDUM §C — wire `detect_frameworks()` first), F13 (affected_tests), F7 (PRF, gated
by P0.3), F1 (feedback, gated by P0.3 + P0.5), F10 (investigate), F12 (temporal, gated), F11
(memory-bank), F14/F15 (guidance), F16 (cache — only if P0.2 done + hit-rate measured), F17
(reranker — only if F7+F1 leave a measured gap).
