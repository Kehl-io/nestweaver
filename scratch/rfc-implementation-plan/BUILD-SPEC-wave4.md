---
title: Build-ready spec — Wave 4 (F7 PRF + F1 feedback) — FINISH, not build
created: 2026-05-29
companion_to: BUILD-SPEC-phase0-wave1.md … wave3.md, IMPLEMENTATION-PLAN.md, ADDENDUM-evidence-and-findings.md
scope: The eval-harness-gated quality features. Re-verification shows BOTH are partially built
       already — this spec is scoped to what actually remains. Gated on P0.3 (eval harness).
---

# Build Spec — Wave 4

> **Meta-finding (important).** The codebase is materially more built-out than the RFC (written
> against the older `5df5c94`) assumes. Verified already-landed in this wave's scope: **interaction
> scores are consumed in PPR** (not "loaded but never consumed" — that finding is now stale),
> **`FollowUp` events + `session_id` exist**, and **alias/synonym query expansion is shipped**. Each
> remaining RFC feature must be re-verified against current code before speccing — which is what
> these waves do. See the per-feature "Current state (verified)" lines below.

Same format: **Goal · Current state · Verified surface · Schema/signatures · Tasks · Tests ·
Acceptance · Effort · Deps**. Both gated on **P0.3 eval harness** before default-on.

---

## F7 — BM25 PRF + taxonomy expansion
- **Goal.** Improve recall on natural-language queries via pseudo-relevance feedback + curated alias
  expansion.
- **Current state (verified).** **Alias/synonym expansion is DONE** — `expand_query_with_aliases`
  (`query.rs`, tested in `expand_query_tests` at `query.rs:1483`) loads aliases
  (`load_alias_sidecar`) and expands at query time. **PRF two-pass term mining is NOT present**
  (no `prf`/`rm3`/feedback-term code found). So F7 = build the PRF pass only.
- **Verified surface.** BM25 candidate ranking in `store/src/tantivy_index.rs::search`; IDF stats
  via Tantivy's `Bm25StatisticsProvider`; first ranking inside
  `query.rs::build_brain_context_hybrid_with_aliases`.
- **Schema/signatures.** Gate behind `--prf` (CLI) / `prf: true` (MCP) + `[ranking] enable_prf`.
  Pass 1: original query → top-K=5 docs. Mine candidate terms with IDF > median; keep top-N=10 by
  `IDF × tf-in-top-K`. Pass 2: append at weight **0.3×**, cap total query length at 64. Surface
  `expansion_terms` in `--debug`.
- **Tasks (ordered).** (1) after the first BM25 ranking, pull top-K=5 bodies; (2) extract/score
  candidate terms by IDF (`Bm25StatisticsProvider`), excluding terms already in the query; (3)
  append top-N at 0.3×; (4) re-run BM25 (pass 2); (5) gate behind flag + config.
- **Tests (TDD).** A natural-language query mines an expected high-IDF term; `--prf` changes the
  top-5; query length capped; alias expansion (already shipped) still passes.
- **Acceptance.** RFC F7 acceptance — `expansion_aliases` already returns (alias path); with `--prf`,
  `sync.md`/`status.md`-type notes surface higher on a "where did we leave off" query, validated on
  **P0.3**.
- **Pitfalls.** **Query drift** (down-weight 0.3×, cap N≤10, prefer high-IDF, consider *selective*
  expansion gated on the pass-1 score gap); per-query inconsistency (keep opt-in). Note: RRF is
  rank-only, so PRF weights affect the **BM25-internal order**, reaching the fused result only via
  changed BM25 ranks — set expectations.
- **Effort.** M. **Deps.** P0.3 (gate). Reported deltas are directional only (+10–30% MAP on weak
  TREC baselines) — **gate on our corpus, don't import the magnitude.**

---

## F1 — Agent feedback loop
- **Goal.** Let success-signalled nodes rank slightly higher next time, safely.
- **Current state (verified) — ~70% built.**
  - **Ask 1 (consume interaction scores in PPR): DONE.** `PprConfig.interaction_scores` +
    `interaction_bias_weight = 0.05` (`algorithms/src/ppr.rs:18-30`); store holds an
    `interaction_cache` set via `load_interaction_cache`, populated at `mcp/src/lib.rs:49` from
    `load_interaction_scores`; the **5% additive blend into the personalization/teleport vector** is
    implemented in `store/src/ranking.rs:599-623` (and mirrored in the pure algorithms PPR).
  - `EventType` (`interactions.rs:51`) = Query / Access / **FollowUp** / Impact — FollowUp
    (access-immediately-after-query) **already auto-detected**; `session_id` **already present**.
  - **Missing:** `TerminalSuccess`; explicit success/negative recording; `interactions show --uid`;
    the "tracking disabled" note in `brain status`; and the **anti-feedback-loop hardening** the
    adversarial review called for.
- **Design decision.** **Do NOT re-implement Ask 1 as a "2.0× multiplicative cap."** The shipped 5%
  additive teleport blend is the principled locus (Haveliwala topic-sensitive PageRank) and is
  *inherently bounded* — strictly safer against runaway than an unbounded-direction 2.0× multiplier.
  Keep it; tune the 5% weight on P0.3 if needed.
- **Verified surface.** `interactions.rs` (events, decay, `load_interaction_scores:456`);
  `mcp/src/lib.rs:49` (load → cache); `ranking.rs:554/599` (blend); `InteractionCommands` in
  `main.rs:1233` (Status/Clear only).
- **Schema/signatures.** Add `EventType::TerminalSuccess` (+ its decay weight). New
  `nestweaver interactions show --uid <uid>`. `brain status` prints `interaction_tracking: disabled
  (run with --track-interactions to enable)` when off.
- **Tasks (ordered).** (1) add `TerminalSuccess` kind; (2) record success (surfaced UID then
  edited/written, or clean session-end with no re-query) and **negative** (next action is another
  search/reformulation) — FollowUp already covers access-after-query; (3) **harden vs feedback-loop
  bias (Ensign 2018): add a uniform exploration floor so non-accessed nodes stay reachable, and let
  the negative signal down-weight** — the shipped 5% blend already bounds magnitude; (4)
  `interactions show --uid`; (5) `brain status` disabled-note.
- **Tests (TDD).** A surface→edit sequence records `TerminalSuccess`; a surface→re-search records a
  negative; the exploration floor keeps a never-accessed node above zero personalization;
  `show --uid` prints the event history; ranking shifts toward success-signalled UIDs across runs
  (on P0.3).
- **Acceptance.** RFC F1 acceptance (`interactions show --top --kind terminal-success`; before/after
  ranking shift) — measured on **P0.3** with tracking toggled.
- **Pitfalls.** Runaway/popularity bias (bounded blend + exploration floor + negative signal +
  decay); `TerminalSuccess` mis-attribution (session walked away ≠ success — weight conservatively);
  **reproducibility** (machine-local scores make benchmarks non-deterministic → always A/B with
  tracking *off* as the baseline).
- **Effort.** **S–M** (consumption already done — much smaller than the RFC implies). **Deps.** P0.3
  (gate), P0.5 (success-signal labeler).

---

## Wave 4 build order

```
P0.3 eval harness ──gate──► F7 PRF pass        (alias expansion already shipped)
P0.3 + P0.5 ───────gate──► F1 finish           (PPR consumption already shipped; add
                                                 TerminalSuccess + negative signal + floor + CLI)
```

Both are **off-by-default** and must clear the P0.3 harness before defaulting on. F7's PRF and F1's
remaining work are independent and can run in parallel once P0.3 exists.

**Remaining after Wave 4:** F10 (investigate — composes F7/F8/F9), F11 (memory-bank — builds on F9),
F14/F15 (guidance, Tier E3), F16 (cache — only if P0.2 done + hit-rate measured; adversarial says
drop-to-experiment), F17 (reranker — only if F7+F1 leave a measured gap; adversarial says
replace-with-simpler). Each will be re-verified against current code at spec time, since (per the
meta-finding) several may be more built than the RFC assumes.
