---
title: Build-ready spec — Wave 3 (F13 affected_tests + F12 git-activity CodeRank)
created: 2026-05-29
companion_to: BUILD-SPEC-phase0-wave1.md, BUILD-SPEC-wave2.md, IMPLEMENTATION-PLAN.md, ADDENDUM-evidence-and-findings.md
scope: The PR-review track. Serves measured journeys J2 (impact) and J5 (PR blast radius). Surface
       re-verified against current code, incl. the new nestweaver-algorithms crate.
---

# Build Spec — Wave 3

Same format: **Goal · Verified surface · Schema/signatures · Tasks · Tests (TDD) · Acceptance ·
Effort · Deps**. Both features share a git-history substrate (W3.0).

**Re-verification highlights (current code):**
- PPR is now **pure** in `crates/nestweaver-algorithms/src/ppr.rs::personalized_pagerank`
  (WASM-compatible). F12's activity multiplier must **not** go there — apply it where
  `pagerank_score` is *consumed* in `crates/nestweaver-store/src/ranking.rs` / `hubs.rs`. This both
  matches the research (post-hoc, not in the fixpoint) and is now architecturally enforced.
- F13 is **mostly reuse**: `store/src/traverse.rs::impact` + `direct_callers_of` (reverse CALLS/
  IMPORTS with `min_confidence`); `engine/src/process.rs::detect_changes_impact` (changed_files →
  affected_symbols); `engine` `changed_files_from_git` (CI fast-path); and **test-file heuristics
  already exist** in `crates/nestweaver-parser/src/entry_points.rs::is_test_file` (`/__tests__/`,
  `*_test.go`, `*_test.dart`, `*_test.exs`, …).
- Existing git surface is **diff-based** (`engine/src/git_diff.rs::detect_changes`,
  `changed_files_from_git`) — there is **no log/history mining** yet (W3.0 is new).

---

## W3.0 — Git-history mining substrate (`git_activity.rs`)
- **Goal.** Per-file commit recency from git history. Feeds F12 (recency multiplier) and F13's
  co-change fallback.
- **Verified surface.** `engine/src/git_diff.rs` has diff/changed-files only; history mining is new.
  New module `crates/nestweaver-engine/src/git_activity.rs`.
- **Schema/signatures.** Run `git log --name-only --pretty=format:%H%x09%aI` per repo at index time.
  `recency_score(path) = mean_{last N=20 touching, non-bulk commits} exp(-Δdays / 180)` using
  **author date `%aI`** (rebases/squash distort commit date). Bulk-commit threshold = **per-repo
  percentile** (commit sizes are heavy-tailed — a fixed ≥500 is wrong for small repos; Hindle MSR
  2008). Detect/skip merge commits.
- **Tasks.** (1) per-repo `git log` sweep behind `--with-git-activity`; (2) compute per-file recency;
  (3) per-repo bulk-commit percentile filter; (4) emit `{path → score ∈ [0,1]}`.
- **Tests (TDD).** Synthetic git fixture: a recently-touched file scores higher than a stale one; a
  file only touched in a bulk commit is excluded from the average.
- **Acceptance.** Scores in `[0,1]`; recent > stale; deterministic on the fixture.
- **Effort.** M. **Deps.** none. **Feeds.** F12, F13(fallback).

---

## F12 — Git-activity-dampened CodeRank
- **Goal.** Demote dormant code at rank-read time so an active service outranks a dormant fork of
  the same name — **without** touching the pure PPR fixpoint.
- **Verified surface.** Pure PPR in `algorithms/src/ppr.rs` (leave it pure). `pagerank_score`
  persisted on nodes (`<db>.pagerank.json` cache + node column); **consumed** in
  `store/src/ranking.rs` and `engine/src/hubs.rs` — apply the multiplier there.
- **Schema/signatures.** Add `git_activity_score: Option<f64>` to the symbol/file node (absent →
  treated as `1.0`, neutral). Config `[ranking] git_activity_weight` + per-repo
  `[ranking] use_git_activity = false`.
- **Key decision (fix RFC §1.4 bug).** `(1 + w·(score − 0.5))` with `score ∈ [0,1]` ranges over
  `[1 − w/2, 1 + w/2]`. The RFC's `[0.4, 1.6]` clamp needs **w = 1.2**, not 0.6 — otherwise the
  clamp never binds. Default `w` to a value consistent with the intended clamp and document it;
  center 0.5 = no change (Temporal PageRank degrade-to-baseline).
- **Tasks.** (1) `nestweaver index --recompute-rank --with-git-activity` populates
  `git_activity_score` from W3.0; (2) at `pagerank_score` read in `ranking.rs`/`hubs.rs`, multiply by
  `clamp(1 + w·(score − 0.5))`; (3) per-repo opt-out; (4) `nestweaver explain rank <uid>` →
  `{base_pagerank, git_activity_score, final_rank}`.
- **Tests (TDD).** Given two same-named symbols (active vs stale repo), active ranks higher after
  recompute; absent score → neutral (rank unchanged); clamp bounds an extreme score.
- **Acceptance.** RFC F12 acceptance (`hubs` before/after shifts toward active; `explain rank`
  populates all three).
- **Effort.** M. **Deps.** W3.0. **Gate.** Ranking-quality gain is `[UNVERIFIED/novel]` — validate
  on the **P0.3 eval harness** before defaulting on; ship behind the flag until it clears.

---

## F13 — `affected_tests`
- **Goal.** changed files → changed symbols → reverse `CALLS`/`IMPORTS` (depth 3) → test files,
  bucketed into priority tiers. "Which tests should this MR run?"
- **Verified surface (reuse).** `store/src/traverse.rs::impact` (`:34`) + `direct_callers_of`
  (`:79`) — reverse traversal with `min_confidence`. `engine/src/process.rs::detect_changes_impact`
  (`:223`) — changed_files → `affected_symbols`. `changed_files_from_git` (engine `lib.rs:80`) — CI
  fast-path. **Test detection** reuses/extends `parser/src/entry_points.rs::is_test_file`.
- **Schema/signatures.** Avoid a new node kind: classify test files at query time via the existing
  heuristics + per-repo `[tests] include=[…]`, `exclude=[…]` overrides. MCP
  `affected_tests(changed_files: [String], base_ref?)` → `{changed_files, changed_symbols,
  tier_1:[{test_file, tests:[…], symbol_uid}], tier_2, tier_3, summary}`. CLI
  `nestweaver affected-tests [--base-ref main] [--json]`.
- **Tasks (ordered).** (1) resolve `changed_files` → symbols via `detect_changes_impact`; (2)
  reverse-traverse to depth 3 via `traverse.rs::impact`; (3) filter reached nodes to those residing
  in **test files** (heuristics + config); (4) bucket by depth — tier-1 directly tests a changed
  symbol, tier-2 tests a tier-1 caller, tier-3 transitive — and order within tier by edge confidence
  (CALLS=1.0 … ACCESSES=0.4); (5) `--base-ref` runs `git diff --name-only base...HEAD` via
  `changed_files_from_git`.
- **Tests (TDD).** A change to a tested symbol yields `tier_1 ≥ 1`; a caller's test lands in tier-2;
  a non-test caller is excluded; `--base-ref` derives the file list itself.
- **Acceptance.** RFC F13 acceptance (`tier_1` length ≥ 1 when the diff touches a tested symbol);
  `/mr-review` skill consumes it.
- **Pitfalls (from RTS literature — Rothermel & Harrold "safe RTS", STARTS, Meta predictive
  selection).** Static call-graph RTS is **not safe** (misses reflection/DI/codegen/data-driven
  tests) — frame as a **measured recall target vs run-all** (Meta's posture), not provable safety;
  **"no path found ≠ safe to skip"** (distinguish from "no tests, high confidence"); warn when tests
  exist beyond the depth-3 cap; deprioritize known-flaky tests via history; for squash/large MRs
  past a change threshold, recommend run-all; offer a conservative full-run fallback.
- **Effort.** M (mostly reuse; new work is tiering + test-node filtering + the changed→symbol glue).
  **Deps.** none for the core (W3.0 only for an optional co-change fallback signal).

---

## Wave 3 build order

```
W3.0 git-history mining ─► F12 (recompute-rank, multiplier at read)   [gate: P0.3 eval harness]
F13 affected_tests  ── independent (reuses traverse.rs::impact + detect_changes_impact
                        + entry_points test heuristics; W3.0 only for co-change fallback)
```

**Sequencing note.** F13 has the fewest dependencies and serves J2/J5 directly — it can start
immediately, in parallel with W3.0. F12 should not default-on until it clears the P0.3 eval harness
(its ranking gain is novel/unverified).

**Next waves (specify when reached):** F7 (PRF) and F1 (interaction feedback) — both **gated on the
P0.3 eval harness landing**; then F10 (investigate, composes F7/F8/F9), F11 (memory-bank, builds on
F9), F14/F15 (guidance — Tier E3), F16 (cache — only if P0.2 done + hit-rate measured), F17
(reranker — only if F7+F1 leave a measured gap). PPR-touching specs (F1) will be re-verified against
the `nestweaver-algorithms` crate at spec time.
