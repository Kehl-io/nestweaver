---
title: Build-ready spec — Final wave (F10 investigate, F11 memory-bank, F14/F15/F16/F17)
created: 2026-05-29
companion_to: BUILD-SPEC-phase0-wave1.md … wave4.md, IMPLEMENTATION-PLAN.md, ADDENDUM-evidence-and-findings.md
scope: Completes build-ready specs for all 17 RFC features. F10/F11 full depth; F14–F17 as
       spec + carried adversarial gates (those four are contingent/gated, not clean go's).
---

# Build Spec — Final wave

Verified: these six are **greenfield** (no existing investigate/bundle, typed note edges,
memory-lint, hooks, static rules block, response cache, or ML runtime dep). F10/F11 are clean
composites of already-shipped work; F14–F17 carry the gates the adversarial pass established.

---

## F10 — `investigate` bundle primitive
- **Goal.** One call → an architectural map (clusters as "domains" + top-PageRank node per cluster
  as "entry points") + `bundle_id` (24h TTL); `investigate_expand` / `investigate_hydrate` drill in.
- **Current state.** Greenfield, but composes shipped primitives: hybrid search, `brain_context`,
  Leiden (`cluster_dispatch.rs`), F8 inline bodies, F7 PRF, token-budget helpers
  (`tools.rs::budgeted_cut`/`render_cost`).
- **Verified surface.** New `crates/nestweaver-engine/src/investigate.rs` + bundle table in the
  `.lbug`. Bundle state keyed/tagged with **`graph_generation`** — needs **P0.2** (persist + bump on
  index) to be valid across processes. MCP result shaping uses `structuredContent` + `resource_link`
  (return entry-point URIs, hydrate later — MCP spec).
- **Schema/signatures.** `bundles { bundle_id, created_at, query, scope, seeds,
  entries:[{asset_id, uid, kind, summary, inline_body?, expanded:bool}] }`, 24h TTL. `asset_id` =
  stable short hash of `(bundle_id, uid)`. MCP `investigate(query, scope?, token_budget?)` →
  `bundle_id` + map; `investigate_expand(bundle_id, targets:[asset_id|uid])`;
  `investigate_hydrate(bundle_id)`. `scope: "project:<slug>" | "repo:<name>" | "vault"` (reuses
  `project_context` member-UID logic for project scope).
- **Tasks (ordered).** (1) bundle store (module + table), tagged with `graph_generation`; (2) map
  construction: PRF hybrid search → top-30 → group Symbol/Note → fetch neighbours via
  `brain_context` → Leiden over the union → top-3 clusters as domains + highest-PageRank node each
  as entry points; (3) inline ≤5 high-confidence bodies (F8); (4) token-budget paging (default 4000,
  cap 16000, return `more_available: <count>`); (5) `expand`/`hydrate`.
- **Tests (TDD).** `investigate` → `entries ≥ 10`, `clusters` 2–3, non-empty `bundle_id`; `expand`
  on an `asset_id` returns a body; budget cap respected (`more_available` set when truncated).
- **Acceptance.** RFC F10 acceptance (bundle shape; expand drilling; ~40–70% fewer tokens vs the
  naive search→context→read loop — measure, don't assume).
- **Pitfalls.** Over-bundling recreates "lost in the middle" → strict budget + hydrate-on-demand;
  stale `bundle_id` within TTL → `graph_generation` tag invalidates; cluster instability across runs.
- **Effort.** M–L. **Deps.** F7, F8, F9 (Leiden-over-vault), P0.2, §2.3 disk layer.

---

## F11 — Memory-bank semantics over the vault
- **Goal.** Typed relationship edges + 7 health checks + a consolidation pipeline +
  `brain_memory_related`.
- **Current state.** Greenfield: only `WIKILINK` edges + 5-priority wikilink resolution exist
  (`index_md.rs:1091`); no typed semantic edges, lint, or consolidation. Orphan/broken-link checks
  **reuse F9**.
- **Verified surface.** Frontmatter parsing in `index_md.rs` (`ParsedNote`); `EdgeType` enum
  (`edges.rs`, additive); F9 doc-graph tools.
- **Schema/signatures.** Add `EdgeType::{Supersedes, DependsOn, CausedBy, RelatesTo}` (additive),
  mapped to **SKOS** (`broader`/`narrower`/`related`) and **PROV-O** (`wasDerivedFrom`/`wasRevisionOf`)
  semantics. Derive from frontmatter (`supersedes: [..]`, `depends_on: [..]`) and from wikilinks
  inside sections named "Supersedes" / "Depends on" / "See also"; ungrouped wikilinks stay generic.
  MCP `brain_memory_lint`, `brain_memory_consolidate(--dry-run|--apply)`,
  `brain_memory_related(uid, edge_types?, depth?)`. CLI `nestweaver memory lint|consolidate|related`.
- **Tasks (ordered).** (1) derive typed edges during `index_md`; (2) **7 checks**: stale
  (`status: active` + >90d no edit), contradictions (Supersedes cycles), orphans + broken_wikilinks
  (**reuse F9**), supersession_chains, schema_drift (frontmatter vs `_templates/<kind>.md`),
  dangling_relationships (typed edge → missing node); (3) consolidation **dry-run by default** —
  e.g. a daily-log H3 linked from ≥3 idea notes and surviving ≥14 days is an `_ideas/` candidate;
  output a manifest the user accepts; (4) `brain_memory_related` typed-edge BFS.
- **Key decision (A-MEM caution).** Consolidation **suggests + records `wasDerivedFrom` provenance,
  never silently mutates** files.
- **Tests (TDD).** A frontmatter `supersedes:` creates a `Supersedes` edge; the stale check flags an
  old `status: active` note; consolidation proposes ≥1 promotion on a fixture; `related` returns
  typed neighbours only (no generic wikilink noise).
- **Acceptance.** RFC F11 acceptance (lint keys present; consolidate proposals ≥1; related typed-only).
- **Pitfalls.** Orphan/stale false-positives for MOC/convention notes (tier-aware allowlist, reuse
  F9); false consolidation (require temporal spread + link threshold); never auto-mutate.
- **Effort.** M–H (consolidation is the long pole). **Deps.** F9.

---

## F14 — Subagent PreToolUse hook (Tier E3 — agent-harness)
- **Goal.** Inject dynamic guidance into spawned subagents via a hook, so guidance lives in one place.
- **Current state.** Greenfield (no hook install; `setup` only configures the MCP).
- **Schema/signatures.** `nestweaver admin instructions [--for-subagent | --set <file> | --reset]`
  (prints subagent text); `nestweaver admin install-hook [--runtime claude|…] [--dry-run]`
  (idempotent matcher on `Task`). Prefer the **`mcp_tool` hook type** (reuse the running server —
  avoids cold-CLI/DB cost per spawn).
- **Tasks.** instruction store (`~/.nestweaver/instructions[.subagent].md`, bundled defaults);
  print subcommand; idempotent hook installer; optional `admin clean` to strip legacy sections.
- **Carried gate (adversarial/research).** **"Control Illusion" (Geng 2025): instruction hierarchies
  aren't reliably obeyed — guidance helps but is not enforcement.** Plus runtime lock-in
  (Claude-Code-specific schema) and untrusted-injection risk. **Posture: build only if §2.6 guidance
  store earns its keep; frame as defense-in-depth, not control.**
- **Effort.** Low–M. **Deps.** §2.6.

---

## F15 — Hard-rule guidance in `generate-guide` (Tier E3)
- **Goal.** A canonical `**HARD RULE:**` block at the top of generated guides; versioned.
- **Current state.** `agent_guide.rs::generate_guide` emits guides (markdown / cursor-rule /
  agents-md) **dynamically from the graph**; no static rules block — add one.
- **Schema/signatures.** Const rule array in `agent_guide.rs` (`--rules-from <file>` override);
  position rules at top with `**HARD RULE:**` prefix; `rules_version: N` in frontmatter.
- **Evidence (carried).** "Enumerate then verify" is **directly backed by Chain-of-Verification**
  (Dhuliawala et al., ACL 2024); top placement by the Instruction Hierarchy (Wallace et al. 2024).
  But rules are *helpful, not guaranteed* (Geng 2025) → keep them **few** (rule-bloat dilutes scarce
  front-of-context space).
- **Tasks.** rule store; inject at top; version field; bump on change.
- **Acceptance.** RFC F15 acceptance (`grep '^\*\*HARD RULE:'` count; `rules_version` in head).
- **Effort.** Low. **Deps.** §2.6.

---

## F16 — Response cache (GATED — adversarial: drop to experiment)
- **Goal.** Cache tool responses, invalidated by file changes.
- **Current state.** Greenfield.
- **Carried verdict (ADDENDUM §C).** **Do NOT build the full machinery as planned.** Premise
  ("two runtimes re-running within 60s") is unmeasured; **correctness is broken daemon-less** —
  `graph_generation` isn't persisted and isn't bumped by `index`, so every short-lived process sees
  `gen=0` and the safety net never fires; and there are **no latency numbers** justifying it.
- **Minimal safe version (the actual recommendation).** (1) Instrument first — log
  `(tool, normalized_args, generation)` + per-tool latency; measure real cross-process repeat rate.
  (2) If latency matters, ship **process-local in-memory memoization** inside the long-lived
  MCP/`watch` process only, keyed on the *in-process* generation (correct by construction, ~1% of
  the complexity). (3) Only consider a disk cache after **P0.2** (persist + bump generation) lands
  *and* a measured hit-rate clears a pre-committed bar; if so: cache table + ZSTD + content-hash
  scope-digest + LRU/TTL + `--no-cache`.
- **Effort.** High (full) / Low (process-local memo). **Deps.** P0.2 (for any disk variant);
  measured hit-rate gate.

---

## F17 — Learned listwise reranker (GATED — adversarial: replace with simpler)
- **Goal.** Re-score top-50 with a learned model, gated to ≥5% nDCG@10 over hybrid.
- **Current state.** Greenfield; **no `candle`/`ort`/`onnx` dependency exists** — a net-new heavy ML
  surface.
- **Carried verdict (ADDENDUM §C).** **Do NOT build the hand-rolled candle net.** Single-user label
  volume can't clear a CI-excludes-zero bar on ~40 queries; it's circular (relearns first-stage
  order via `rank_position` + labels from the current ranker); and **F7 + F1 draw from the same
  signal and ship first**, so F17 must beat an already-feedback-boosted baseline.
- **Cheaper alternatives (the recommendation).** (1) A **hand-tuned monotonic scoring function** over
  the same features (extend F6's prior machinery — explainable, no ML lifecycle); or (2) if a model
  is truly wanted, a **LambdaMART tree** (XGBoost `rank:ndcg` → ONNX) — sample-efficient, the
  research's own recommended baseline.
- **Posture.** Ship F6/F7/F1, measure on **P0.3**. Only if a *durable, reorder-specific* gap remains
  with enough labels, run a LambdaMART tree as a **one-off gated experiment** (default-off, CI bar);
  permanent close if it fails. The hand-rolled 20K-param net is not justified.
- **Effort.** M (if pursued). **Deps.** F7, F1, P0.3. **Gate.** ≥5% nDCG@10, CI excludes zero, on P0.3.

---

## Final-wave build order

```
F9 ─► F11 (typed edges + lint + consolidation)
F7,F8,F9 + P0.2 ─► F10 (investigate)
§2.6 ─► F14, F15 (guidance — Tier E3, defense-in-depth)
P0.2 + measured hit-rate ─► F16 (start: instrument → process-local memo only)
F6,F7,F1 + P0.3 ─► F17 (only if a measured reorder gap remains → LambdaMART experiment, not the net)
```

**All 17 features now have build-ready specs** across `BUILD-SPEC-phase0-wave1.md` … `wave5-final.md`.
The four gated features (F14/F15/F16/F17) carry their adversarial postures so a build decision is
made with eyes open — but per the standing constraint, the build/defer/kill call itself remains yours.
