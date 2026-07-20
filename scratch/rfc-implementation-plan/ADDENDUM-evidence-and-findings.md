---
title: Addendum — Evidence, Verified Surface, and Adversarial Findings
created: 2026-05-29
companion_to: IMPLEMENTATION-PLAN.md
note: This closes gaps #1 (evidence), #2 (verified surface), #3 (adversarial). It deliberately does NOT make build/defer/kill calls (#4) — that decision is the user's. Everything here is decision *input*.
---

# Addendum: what the evidence, the code, and the skeptics actually say

## A. The headline finding (gap #1 — evidence mining)

### A.1 — In the actual traces, the tool is not in the loop
Across **every** Claude Code session under `~/.claude/projects/` on this machine:

| Signal | Count |
|--------|-------|
| NestWeaver MCP tool invocations | **0** |
| `Bash` tool calls | **39,668** |
| Other MCP actually used | Claude-in-Chrome (183), Supabase (~38), Gmail/Drive (~11) |
| Files *mentioning* "nestweaver" (as text/paths) | 665 |

In day-to-day captured usage, the agents orient via **Bash (grep/rg/git/find)**, not via NestWeaver. NestWeaver appears as *the thing being built*, not *a tool being used*.

**Resolved (user-confirmed): NestWeaver's MCP is NOT configured in these projects.** So the
zero-usage signal is a **configuration gap, not abandonment** — the value question stays open, not
answered negatively. **Step zero is therefore to configure NestWeaver's MCP (`nestweaver setup
claude-code`) and actually dogfood it.** No feature in this plan delivers value until the tool is
in the loop, and we'll have no real usage evidence to prioritize by until it is.

**Other caveats:**
- The `journeys.sh` benchmark drives the NestWeaver **CLI** as a subprocess, which would *not* appear as an `mcp__` tool call — so CLI/benchmark usage is invisible to this count.
- The RFC itself claims **89–96% token savings** from a NestWeaver orientation pass vs historical sessions — but those sessions live in `~/.claude/projects/-Users-kkehl-...Obsidian-Work/`, which is **not on this machine**, so I can't reproduce that evidence here.

**Implication (the part that matters):** the precondition for the whole feature track is **getting
NestWeaver's MCP configured and dogfooded** — that's now confirmed as the gap, not a quality
failure. Once it's in the loop, the features most aligned with *keeping agents on the graph instead
of falling back to Bash* — F3/F4 (first-party regex/count so skills stop shelling out) and F5
(symbol-window reads vs whole-file `Read`) — are the adoption levers, not just efficiency wins.
Prioritize them on that evidence, not taste.

### A.2 — Your own benchmark defines value as 7 code-graph journeys
`_docs/benchmark/journeys.sh` measures these, by **latency + result count** (not retrieval quality), vs Graphify:

| # | Journey | CLI exercised | Features that serve it |
|---|---------|---------------|------------------------|
| J1 | "What does this do?" (symbol understanding) | `symbol` | **F5, F8** |
| J2 | "What breaks if I change this?" (impact) | `impact` | **F13**, F2, F12 |
| J3 | "How is this organized?" (architecture) | `repo-map/hubs/clusters/summary` | **F10**, F9, F12 |
| J4 | "Find where X is used" (cross-file refs) | `context` | F2, F3 |
| J5 | "What changed / what's at risk?" (PR review) | `pr-impact` | **F13**, F12 |
| J6 | "Summarize this area" (token-efficient) | `summary` | **F5, F8** |
| J7 | "Is anything dead?" | `dead-code` | (exists) F12 |
| **J8** | **"Where are we on project X?" (vault Project orientation — NEW, NestWeaver-only)** | `project-context` / `brain context` | **F11, F10, F8**, F6, F7, F1 (+ Bug #12 guard) |

### A.3 — Evidence-tier ranking (this is data, NOT a cut list)
Where each feature sits relative to *measured* value:

- **Tier E1 — directly serves a benchmarked journey (value is measurable today):**
  F5 (J1/J6), F8 (J1/J6/**J8**), F13 (J2/J5), F10 (J3/**J8**), F3+F4 (J4 + Bash-displacement),
  F9 (J3), **F11 (J8 — vault Project orientation)**, F2 (J2/J4 — *if scoped per §C*).
- **Tier E2 — improves journeys but is UNMEASURABLE with current infra:** F6, F7, F1, F12, F17.
  The journeys benchmark (incl. J8) measures *speed + coverage + Bug-#12-style notes-surfaced*,
  **not *ranking quality*** — so these features' relevance gains can't be *seen* until the §2.7
  quality harness exists. Not just speculative; currently un-instrumented.
- **Tier E3 — outside the measured journey set entirely (net-new surface):** F14/F15 (agent-harness
  guidance), F16 (cache). *(F11 moved up to E1 — it now serves Journey 8.)*

### A.4 — Resolved: the vault Project is now a critical journey (J8)
Originally the benchmark was **code-graph only**, with no vault/Project journey — despite that
being the RFC's headline use case (and where Bug #12 lived). **User decision: the vault Project is
a real, first-class use case.** It is now **Journey 8 (Project Orientation)** in
`_docs/benchmark/journeys.sh` — NestWeaver-only, since competitors don't model a vault Project
spanning repos. It measures `project-context`/`brain context` latency, notes-surfaced, and
locations, and **bakes in the Bug #12 metric (a project that declares repos must surface its
notes) as a regression guard.** NestWeaver is a code-**and**-vault tool, and project orientation is
the differentiator — so the vault-retrieval capabilities (F11, F10, F8) move onto **measured
ground** (A.3). Run it with `NW_PROJECT_DB` + `NW_PROJECT` set.

---

## B. Verified implementation surface (gap #2 — corrections to the plan)

Each verified against real code by read-only agents.

| Plan claim | Verdict | Reality | Effort impact |
|-----------|---------|---------|---------------|
| F7 lives in `nestweaver-store/src/bm25.rs` | **WRONG** | No `bm25.rs`. BM25 = `tantivy_index.rs`; RRF fusion = `query.rs::rrf_fuse` (~`query.rs:1001`, k=60) | Plan error; corrected in main doc |
| `Symbol` has no `end_line` | **CONFIRMED** | `nodes.rs:140`; parser calls `start_position()` only (`parse.rs:593`), discards available `end_position()` | F5/F3-sym/F8-sym blocked on a plumbing add (RawSymbol→Symbol→DB→persist) |
| `graph_generation` gives a cache-invalidation key (§2.3) | **WRONG (load-bearing)** | In-memory `AtomicU64::new(0)`, **not persisted**, reset to 0 every open; bumped **only** by the two watcher loops, **never by `index`** (`db.rs:37/53/69/108`, `watcher.rs:341`, `watch_code.rs:271`) | **Voids the daemon-less cache premise.** To use it: persist it + bump on `index`. Otherwise F16/§2.3 cross-process keying can't work |
| Interaction sidecar needs `session_id` added (F1) | **PARTIAL — already done** | `InteractionEvent` already has `session_id`; `EventType` = Query/Access/FollowUp/Impact | F1 cheaper than planned — only add `TerminalSuccess` |
| `Symbol.framework_hint` exists; populate for F2 | **CONFIRMED + dead infra** | Field exists; `detect_frameworks()` (Spring/Flask/Express/etc., `frameworks.rs`) is **exported but never called** — always `None` | Wiring `detect_frameworks()` in is a cheap **enabler** for F2 handler detection |
| F2 "wire into existing `cross_repo_contracts`" | **WRONG** | The live tool (`tools.rs:1445`) does name/import matching via `store.cross_repo_links`. The real contract matcher `find_cross_repo_links` (`resolver/src/cross_repo.rs`) is **unwired dead code** (only its own tests call it) | F2 is **greenfield**, not integration — re-budget |
| F9/F10 "reuse Leiden clusters" | **PARTIAL** | `cluster_dispatch.rs` runs Leiden but **hardcoded to code Symbol edges** (8 edge types). Wikilink (Note↔Note) subgraph needs a new `load_note_edges` (~50 LOC) | Algorithm reusable; graph-loading is new |
| F9 broken-link/orphan/tag-cooccurrence "mostly reuse" | **GREENFIELD** | Only `count_wikilink_edges` + `wikilink_sources_to_note` exist; no orphan/broken/co-occurrence queries | More new code than credited |
| F5 comment stripping "walk tree-sitter comment nodes" | **WRONG** | Grammar queries don't reference `comment` nodes; comments aren't extracted today | Stripping is more work; default-off remains right |
| F15 "const rule array in `guide.rs`" | **PARTIAL** | No `guide.rs`; `agent_guide.rs` generates guidance **dynamically** from the graph, no static rule store | F15 rule store is greenfield |
| F8 `inline_body` | **CONFIRMED** | `BrainNode{uid,kind,title,location,relevance}`; add field + populate in `render_brain_node` + serialize | ~15 LOC, no blocker |
| §2.7 "no quality harness exists" | **CONFIRMED** | `benches/brain_benchmarks.rs` is criterion **speed** only; no nDCG/precision anywhere | Harness must be built |
| Trigram greenfield; `regex`/`regex-syntax` available | **CONFIRMED** | No ngram index in store; deps present in `Cargo.lock` | F3 as planned |

---

## C. Adversarial findings (gap #3 — decision input, not cut calls)

Each feature got an agent whose job was to *refute* the approach. Summaries; full reviews were returned to the orchestrator.

### F2 — static contract linking → **recommended posture: scope down, measure demand first**
- The ~0.79 F1 ceiling (Schneider et al. 2024) is **disqualifying for the headline promise**, not a caveat: in an *impact* tool, false edges erode trust in all edges, and "no edge ≠ safe" concedes the core question.
- The unambiguous lanes are safe and valuable: **gRPC `Service/Method`** (no templating), **`operationId`-matched typed clients**, **same-repo Spring/NestJS handler `IMPLEMENTS` edges**, and **spec→`Contract` nodes + "declared-but-not-implemented" drift diagnostics**.
- The headline general **HTTP-literal cross-repo `CONSUMES`** matcher (Express/Go/`fetch`) is the part the evidence says can't be made trustworthy — generic-path collisions (`/health`, `/users/{id}`), base-path recall, generated clients.
- Recommend: lead with the low-ambiguity lanes; label every edge a confidence-scored *hypothesis*; **count actual cross-repo couplings in the real repos before investing** (n=1 demand is unquantified); budget greenfield (the existing matcher is unwired).

### F16 — daemon-less response cache → **recommended posture: drop to a measured experiment**
- Premise ("two runtimes re-running within 60s") is **unevidenced**; the plan itself says "measure hit-rate before building."
- **Correctness is broken daemon-less:** `graph_generation` isn't persisted and isn't bumped by `index`, so every short-lived process sees `gen=0` → the generation-mismatch safety net **never fires** → correctness rests entirely on the riskiest component (scope-digest). A stale code-graph answer is worse than a slow one (it misleads edits).
- **No latency numbers exist anywhere** — likely optimizing tens of ms against multi-second LLM round-trips.
- Minimal safe version: **process-local in-memory memoization** inside the long-lived MCP/watch process only (in-process generation works there). Instrument real hit-rate + per-tool latency first.

### F17 — learned listwise reranker → **recommended posture: drop the hand-rolled net; keep at most a gated tree experiment**
- Single-user label volume can't train even a 20K-param listwise model to clear a **CI-excludes-zero** bar on ~40 queries.
- **Circularity:** labels come from what the current ranker surfaced, and `rank_position` is a feature → the model's lowest-loss behavior is to reproduce first-stage order (~0% lift by construction).
- **F7 + F1 draw from the same signal and ship first** → F17 must beat an already-feedback-boosted baseline (strictly harder).
- Cheaper alternative that captures most upside: a **hand-tuned monotonic scoring function** (extend F6's prior machinery over the same features, fully explainable) or, if a model is truly wanted, a **LambdaMART tree** (sample-efficient, the dossier's own recommended baseline).

---

## D. What this changes for your decision (still not making the cut)

1. **Configuration + dogfooding is step zero (now confirmed).** The tool isn't in the loop because
   its MCP isn't configured — so `nestweaver setup claude-code` + real dogfooding must come first;
   it's also the only way to generate the usage evidence to prioritize the rest. F3/F4/F5 double as
   the levers that keep agents on the graph rather than Bash once it's wired in.
1b. **The vault Project is now Journey 8** (added to `journeys.sh`, Bug-#12-guarded). This puts
   F11/F10/F8 on measured ground and settles the "code vs vault" identity: it's both, with project
   orientation as the differentiator.
2. **§2.3 / Phase 0 changes.** "Use `graph_generation` as a cache key" is void as-is. Either add a small enabler (persist it + bump on `index`) or drop the cross-process cache premise. This also touches F10's bundle-TTL keying.
3. **F2 is greenfield, not integration**, and its safe value is concentrated in gRPC + specs + same-repo handlers; wiring the dead `detect_frameworks()` is a cheap prerequisite.
4. **Tier E2 features (F6/F7/F1/F12/F17) can't be evaluated** until §2.7 exists — so the eval harness is a true prerequisite for that whole cohort, not a nice-to-have.
5. **The cut itself (build/defer/kill) is yours** — this addendum is the evidence to make it well.
