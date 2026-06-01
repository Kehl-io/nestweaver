# Research Foundation — Agentic Orchestration, Guardrails & Response Cache

Research backing for four NestWeaver RFC features. NestWeaver is a Rust code-and-notes
graph intelligence tool: embedded graph DB (LadybugDB), PPR + BM25 + vectors fused via
RRF, MCP server consumed by short-lived AI coding-agent processes (Claude Code, Cursor,
Codex). No always-on daemon.

Conventions in this doc:
- **[EVIDENCE]** = backed by a cited, retrieved source (paper/spec/official docs).
- **[PRACTICE]** = common engineering practice, not a specific cited result.
- **[UNVERIFIED]** = claim I could not confirm against a primary source.
- All URLs below were retrieved during this research (May 2026).

---

## FEATURE 10 — `investigate` bundle primitive

One MCP call returns an "architectural map" for a free-text topic by composing existing
primitives (hybrid search → group by symbol/note → fetch neighbors → cluster → return top
clusters as "domains" + highest-PageRank node per cluster as "entry points"), with a
`bundle_id` + 24h TTL and follow-up `investigate_expand` / `investigate_hydrate`. Goal:
collapse the 6–12 round-trip "orient me on X" pattern.

### (1) Research foundation

- **RAG / retrieval-augmented generation** — Lewis, Perez, Piktus et al. (Facebook AI / UCL),
  NeurIPS 2020. "Retrieval-Augmented Generation for Knowledge-Intensive NLP Tasks."
  https://arxiv.org/abs/2005.11401 (PDF: https://arxiv.org/pdf/2005.11401).
  [EVIDENCE] Establishes the parametric-LLM + non-parametric-retrieval pattern; result of
  record: RAG generates "more specific, diverse and factual language" than parametric-only
  baselines. Grounds the basic premise that a graph-backed retrieval bundle improves agent
  factuality vs. asking the model to recall architecture from memory.

- **Lost in the Middle** — Liu, Lin, Hewitt, Paranjape, Bevilacqua, Petroni, Liang
  (Stanford et al.), TACL 2024 (arXiv July 2023). https://arxiv.org/abs/2307.03172
  (ACL: https://aclanthology.org/2024.tacl-1.9/). [EVIDENCE] Key result: model accuracy is
  highest when relevant info is at the **beginning or end** of context and degrades sharply
  when it sits in the middle — even for long-context models. **This is the core motivation
  for tiered bundling**: return a small, ranked, front-loaded "domains + entry points"
  summary rather than dumping a large flat result. Put the most important nodes first/last.

- **HyDE (Hypothetical Document Embeddings)** — Gao, Ma, Lin, Callan, ACL 2023.
  "Precise Zero-Shot Dense Retrieval without Relevance Labels."
  https://arxiv.org/abs/2212.10496 (ACL: https://aclanthology.org/2023.acl-long.99/).
  [EVIDENCE] LLM generates a hypothetical answer doc, which is embedded and used as the
  retrieval query; outperforms unsupervised Contriever zero-shot. Relevant as an *optional*
  query-expansion step for free-text topics that don't lexically match symbol names.

- **Agentic RAG survey** — Singh et al., "Agentic Retrieval-Augmented Generation: A Survey
  on Agentic RAG," 2025. https://arxiv.org/html/2501.09136v4 (companion repo:
  https://github.com/asinghcsu/AgenticRAG-Survey). [EVIDENCE] Names four agentic design
  patterns (Reflection, Planning, Tool Use, Multi-Agent) and frames **planning / query
  decomposition** as decomposing a complex task into manageable subtasks for multi-hop
  retrieval. The `investigate` bundle is essentially *server-side* planning: it does the
  decomposition (search → group → expand → cluster) once so the agent doesn't have to drive
  6–12 hops. MA-RAG (https://arxiv.org/abs/2505.20096) is a concrete instance of a Planner
  agent that disambiguates and decomposes queries.

- **MCP tools spec (2025-06-18)** — official. https://modelcontextprotocol.io/specification/2025-06-18/server/tools
  [EVIDENCE] Spec-confirmed fields that the bundle should use:
  - Tool results carry `content` (unstructured: text/image/audio/`resource_link`/embedded
    `resource`) and an optional `structuredContent` JSON object (with optional `outputSchema`
    for validation). For backwards compat, a tool returning `structuredContent` SHOULD also
    serialize the JSON into a TextContent block.
  - `resource_link` content type lets a tool return a URI pointer (e.g.
    `file:///project/src/main.rs`) instead of inlining a whole file — ideal for "entry points"
    that the agent can hydrate later.
  - Content items support `annotations` with `audience` (`user`/`assistant`) and `priority`
    (0–1) and `lastModified` — usable to rank/flag the most important bundle items.
  - `tools/list` (not `tools/call`) is the spec's paginated operation via opaque `cursor` /
    `nextCursor`. **The spec does NOT define cursor pagination for `tools/call` results** —
    so `investigate`'s paging must be an application-level convention (a `bundle_id` + page
    token in args), which is exactly the proposed design.

### (2) Prior art / projects + tradeoffs

- **Aider repo-map** (https://aider.chat/docs/repomap.html) — builds a ranked, token-budgeted
  map of a repo using a graph-ranking algorithm over symbols (PageRank-like) to pick the most
  important definitions within a token budget. [EVIDENCE-of-pattern, from project docs]
  Closest prior art to NestWeaver's own `repo-map`; validates "PageRank-rank + token-budget"
  as the right shape for orientation. Tradeoff: Aider's map is whole-repo, not topic-scoped;
  `investigate` adds topic seeding via hybrid search.
- **GraphRAG (Microsoft)** — community detection over an entity graph, summarize each
  community, answer global queries from community summaries
  (https://github.com/microsoft/graphrag). [EVIDENCE-of-pattern] Directly mirrors the
  proposed "cluster → top clusters as domains" step. Tradeoff: GraphRAG precomputes
  community summaries with LLM calls (expensive, batch); NestWeaver can cluster on the
  existing graph cheaply and label clusters by highest-PageRank node rather than LLM summary.
- **MCP cursor pagination** (https://modelcontextprotocol.io/specification/2025-06-18/server/utilities/pagination)
  — opaque-cursor model; server decides page size. Good template for `investigate_expand`'s
  paging even though it's app-level for tool *results*.

Tradeoff summary: composing existing primitives server-side (bundle) trades a small amount
of server complexity (state for `bundle_id`, TTL) for a large reduction in round-trips and
in mid-context dilution. The alternative — keeping primitives separate and letting the agent
orchestrate — is simpler to build but is exactly the 6–12-hop problem the RFC targets.

### (3) Recommended approach for NestWeaver

- Compose, don't reinvent: `investigate(topic)` = hybrid search (RRF over BM25+vector+PPR,
  already in `nestweaver-store`) → group hits by symbol/note → expand 1-hop neighbors over
  weighted edges (CALLS 1.0 / IMPORTS 0.8 / USES 0.5 / ACCESSES 0.4, already defined) →
  cluster (reuse `clusters.json` / community detection) → return top-K clusters as "domains",
  each labeled by its highest-PageRank node ("entry point"). Grounded in GraphRAG's
  community-summary pattern and Aider's PageRank-ranked map.
- **Token budget is a hard requirement, not a nicety**, justified by *Lost in the Middle*:
  return a tight, ranked summary (domains + entry points first), use MCP `resource_link` for
  files/symbols so bodies are *hydrated on demand* via `investigate_hydrate`, and front-load
  the highest-`priority`-annotated items. Default to a small budget (e.g. ~1.5–2K tokens for
  the orientation layer) and expand only on request.
- Persist bundle state (`bundle_id` → seed set, ranked domains, page cursors) **on disk** in
  the DB with a 24h TTL — consistent with the daemon-less constraint (see Feature 16). The
  follow-ups `investigate_expand`/`investigate_hydrate` reference the `bundle_id` and a page
  token; this is the app-level paging the MCP spec leaves to the implementation.
- Optionally gate a HyDE-style query expansion behind a flag for low-lexical-overlap topics;
  it adds an LLM call NestWeaver doesn't otherwise need, so keep it off by default.

### (4) Pitfalls / failure modes + mitigations

- **Over-bundling** (returning too much, re-creating the mid-context problem). Mitigation:
  strict token budget; tiered hydration; cap domains/entry-points (e.g. top 5–7 domains).
  [EVIDENCE: Lost in the Middle].
- **Stale `bundle_id`** after files change within the 24h TTL → expand returns nodes that no
  longer exist. Mitigation: tag bundle with the `graph_generation` counter / db_mtime bucket
  (NestWeaver already has a `graph_generation` counter per commit d9bc01d); invalidate or
  flag the bundle when generation advances. Shares machinery with Feature 16.
- **Cluster instability** (small graph perturbations reshuffle community labels between
  calls → confusing "domains"). Mitigation: deterministic seeds; reuse persisted
  `clusters.json` rather than re-clustering per call; only recluster on graph-generation bump.
- **Topic with no good lexical/semantic seed** → empty or noise domains. Mitigation: fall
  back to PPR from nearest matches; surface a low-confidence flag; optional HyDE expansion.

### (5) Complexity / effort

Medium-high. Most sub-steps already exist (hybrid search, neighbors, clustering, PageRank);
the new work is (a) the composition/ranking layer, (b) token-budgeted serialization into MCP
`structuredContent` + `resource_link`, and (c) disk-resident `bundle_id`/TTL/paging state.
The state layer overlaps with Feature 16's cache, so build them together.

---

## FEATURE 14 — Subagent PreToolUse hook for guidance injection

A hook on the Task/Agent (subagent-spawning) tool that runs a NestWeaver CLI command to
inject dynamic guidance into a spawned subagent's prompt, so guidance lives in one place
(NestWeaver) instead of stale per-runtime instruction files.

### (1) Research foundation

- **Claude Code hooks reference** — official Anthropic docs. https://code.claude.com/docs/en/hooks
  [EVIDENCE] Verified PreToolUse mechanics directly relevant to this feature:
  - PreToolUse hooks receive JSON on stdin (command hooks) including `session_id`,
    `transcript_path`, `cwd`, `permission_mode`, `hook_event_name` (`"PreToolUse"`),
    `tool_name`, and `tool_input` (the tool's arguments).
  - Output is via a `hookSpecificOutput` object. Relevant fields:
    `hookEventName: "PreToolUse"`, `permissionDecision` (`allow`/`deny`/`ask`/`defer`),
    `permissionDecisionReason`, **`additionalContext`** (string injected into Claude's
    context at the tool-result point), `updatedToolInput` (object replacing tool input
    before execution), and `permissionRules`.
  - Hook `type` can be **`"command"`** (run a shell script — i.e. a NestWeaver CLI call),
    plus `http`, `mcp_tool`, `prompt`, `agent`.
  - The **Task/subagent/Agent tools appear as regular tools in PreToolUse**, matchable like
    any other (`"matcher": "Task"`); there is also a separate `SubagentStart` event.
  - Optional `if` field (permission-rule syntax) narrows when the hook fires.
  - Exit 0 → stdout parsed for JSON; exit 2 → blocking error, stderr fed to Claude;
    other exit codes → non-blocking, stderr to transcript.
  This confirms the proposed design is mechanically supported: a `PreToolUse` hook matching
  `Task` runs `nestweaver <something>` and emits `additionalContext` to inject guidance.

- **Instruction Hierarchy** — Wallace et al. (OpenAI), 2024. https://arxiv.org/abs/2404.13208
  (OpenAI post: https://openai.com/index/the-instruction-hierarchy/). [EVIDENCE] Models
  trained with hierarchy awareness are more robust to lower-privilege instructions
  overriding higher ones. Relevant because injected guidance is *system/developer-level*
  context the subagent should privilege — but see the failure-mode paper below.

### (2) Prior art / projects + tradeoffs

- **PostToolUse `additionalContext` pattern** — community hooks return `additionalContext`
  to feed the agent post-execution (https://github.com/disler/claude-code-hooks-mastery).
  [EVIDENCE-of-pattern] Confirms dynamic injection is an established, in-the-wild use.
- **CLAUDE.md / per-runtime instruction files** — the status quo the RFC replaces. Tradeoff:
  static files are simple and universally read, but go stale and are duplicated per tool
  runtime; a hook computing guidance from the live index is single-source-of-truth but
  Claude-Code-specific (other runtimes — Cursor, Codex — don't share the same hook schema).
- **MCP `mcp_tool` hook type** (from the hooks reference) — lets the hook call an MCP tool
  instead of a CLI. NestWeaver already ships an MCP server, so the guidance could be an MCP
  tool rather than a bare CLI invocation — fewer process spawns, reuses the open DB handle.

### (3) Recommended approach for NestWeaver

- Implement guidance generation as a small, fast NestWeaver subcommand (e.g.
  `nestweaver subagent-guidance`) that reads `tool_input`/`cwd` from stdin and emits JSON
  with `hookSpecificOutput.additionalContext`. Keep `permissionDecision: "allow"` (this is
  enrichment, not a gate).
- Prefer the **`mcp_tool` hook type** over `command` if the per-spawn cost of booting the
  CLI + opening the DB is non-trivial — it reuses the already-running MCP server and the open
  graph. (NestWeaver agents are short-lived, so a cold CLI per Task spawn could be wasteful.)
- Ship the hook config as part of `nestweaver setup claude-code` so it's installed
  automatically; document the JSON contract so other runtimes can adopt equivalents.
- Treat injected guidance as *developer-priority* instructions and phrase them as such
  (consistent with Instruction Hierarchy), but do not assume they're inviolable (next point).

### (4) Pitfalls / failure modes + mitigations

- **Guidance not actually obeyed.** [EVIDENCE] "Control Illusion: The Failure of Instruction
  Hierarchies in LLMs" — Geng et al., 2025 (https://arxiv.org/abs/2502.15851 /
  https://arxiv.org/html/2502.15851v1) — finds across six SOTA models that system/user
  separation "fails to establish a reliable instruction hierarchy" and that social framings
  (authority/expertise/consensus) influence behavior *more* than role hierarchy. Mitigation:
  don't rely on injection alone for correctness-critical behavior; phrase guidance with
  authority/consensus framing; keep guidance short and front-loaded; verify outcomes where it
  matters rather than assuming compliance.
- **Hook latency on every Task spawn.** Mitigation: cache guidance (Feature 16), keep the
  command sub-50ms, prefer `mcp_tool` over cold CLI; use the `if` matcher to scope.
- **Runtime lock-in.** The hook schema is Claude-Code-specific. Mitigation: keep guidance
  generation in a runtime-neutral CLI/MCP tool; the hook is just one adapter.
- **Injection-of-injections / trust.** `additionalContext` is trusted context fed to the
  subagent; if the guidance is derived from indexed *user content* (notes, code comments),
  it could carry adversarial text. Mitigation: derive guidance from NestWeaver-authored
  templates + structured graph facts, not free-text note bodies; sanitize.
- **Silent failure.** [EVIDENCE: hooks ref] A non-2 error exit is non-blocking and only goes
  to the transcript — guidance could silently vanish. Mitigation: monitor/log; fail loud in
  CI for the setup path.

### (5) Complexity / effort

Low-medium. The hook contract is small and well-documented; the work is a fast guidance
subcommand + `setup` wiring + tests. Main risk is latency, mitigated by Feature 16 and/or
the `mcp_tool` hook type.

---

## FEATURE 15 — Hard-rule guidance in generated agent guides

Bake explicit behavioral rules ("if you reference a file path, read it first"; "for 'every X'
questions, enumerate then verify") at the top of generated guides with a **HARD RULE:**
prefix, versioned.

### (1) Research foundation — does explicit rule-stating actually help?

- **Chain-of-Verification (CoVe)** — Dhuliawala, Komeili, Xu, Raileanu, Li, Celikyilmaz,
  Weston (Meta AI), ACL 2024 Findings (arXiv 2309.11495). https://arxiv.org/abs/2309.11495
  (ACL: https://aclanthology.org/2024.findings-acl.212/). [EVIDENCE] Method: draft → plan
  verification questions → answer them independently → produce verified final response.
  Result: **decreases hallucinations** across list-based (Wikidata), closed-book MultiSpanQA,
  and longform generation. This is the *direct* evidence base for the "enumerate then verify"
  hard rule — the RFC's rule is a lightweight, prompt-level instantiation of CoVe's
  draft-then-verify loop, and CoVe shows the loop measurably reduces fabrication.

- **Instruction Hierarchy** — Wallace et al. (OpenAI) 2024, https://arxiv.org/abs/2404.13208.
  [EVIDENCE] Supports placing rules at *developer/system* priority and shows hierarchy-aware
  training improves adherence (up to ~63% better attack resistance reported in coverage),
  motivating the "top-positioned, prefixed, privileged" placement.

- **Counter-evidence (must be stated honestly):** "Control Illusion: The Failure of
  Instruction Hierarchies in LLMs," Geng et al. 2025 (https://arxiv.org/abs/2502.15851).
  [EVIDENCE] Models "struggle with consistent instruction prioritization, even for simple
  formatting conflicts"; system/user separation alone is not a reliable hierarchy. Implication
  for Feature 15: **hard rules help but are not guaranteed-obeyed** — they reduce, not
  eliminate, the failure modes. Evidence is "directionally supportive," not "rules guarantee
  behavior."

- **Prompt-injection robustness** — "Evaluating the Instruction-Following Robustness of LLMs
  to Prompt Injection," 2023 (https://arxiv.org/pdf/2308.10819). [EVIDENCE] Instruction
  following is non-deterministic and bypassable; reinforces that rules are probabilistic.

### (2) Prior art / projects + tradeoffs

- **NeMo Guardrails** (NVIDIA) — programmable rails (input/dialog/retrieval/execution/output)
  via the Colang DSL; Apache-2.0; v0.20.0 (Jan 2026). https://github.com/NVIDIA-NeMo/Guardrails,
  https://docs.nvidia.com/nemo/guardrails/. [EVIDENCE] Heavyweight, runtime-enforced rails —
  the opposite end of the spectrum from prompt-level rules: stronger enforcement, far more
  infra. Tradeoff: NestWeaver's hard rules are prompt-text only (cheap, portable, no
  runtime), accepting weaker guarantees.
- **Guardrails AI** (https://guardrailsai.com) — validators/output-schema enforcement.
  [EVIDENCE-of-existence] Same tradeoff axis: validation > instruction text in strength and cost.
- **OWASP LLM Prompt Injection Prevention Cheat Sheet** — recommends instruction hierarchy +
  delimiters + defense-in-depth (https://cheatsheetseries.owasp.org/cheatsheets/LLM_Prompt_Injection_Prevention_Cheat_Sheet.html).
  [EVIDENCE] Supports XML/delimiter framing for rules and explicitly says rules are *one
  layer*, not a complete defense.
- **Cursor rules / AGENTS.md / Claude Code skills** — the formats NestWeaver's
  `generate-guide` already targets. Versioned, top-positioned rules fit these conventions.

### (3) Recommended approach for NestWeaver

- Ship a small, **versioned** rule set at the top of every generated guide with a clear
  `**HARD RULE:**` prefix and developer-authority framing (Instruction Hierarchy + the
  "social framing beats role" finding from Geng et al. both argue for authoritative phrasing).
- Make the "enumerate then verify" rule explicit and CoVe-shaped (enumerate candidates →
  verify each against the index before answering) since that's the rule with the strongest
  empirical backing.
- Keep rules **few and high-value** — every rule competes for the front-of-context slot that
  *Lost in the Middle* shows is scarce and high-leverage. Don't dilute.
- Version the rule block (e.g. `rules_version: N` in front matter) so guides can be diffed
  and regenerated; tie to NestWeaver release.
- Frame honestly in the RFC: hard rules are **evidence-backed as helpful (CoVe, Instruction
  Hierarchy) but not as guaranteed (Geng et al., 2308.10819)** — they're a cheap, portable
  reliability boost, not enforcement.

### (4) Pitfalls / failure modes + mitigations

- **Guardrail brittleness / non-compliance** [EVIDENCE: 2502.15851, 2308.10819]. Mitigation:
  treat rules as defense-in-depth (OWASP); pair correctness-critical rules with actual
  verification (e.g. a hook or tool that enforces "file read before reference").
- **Rule bloat → mid-context dilution** [EVIDENCE: Lost in the Middle]. Mitigation: cap rule
  count; keep at very top; measure.
- **Staleness across runtimes** — same single-source problem Feature 14 addresses. Mitigation:
  generate rules from one versioned source in NestWeaver, regenerate all guide formats together.
- **Over-trust by maintainers** (assuming the rule == the behavior). Mitigation: document the
  probabilistic nature; don't gate destructive actions on a rule alone.

### (5) Complexity / effort

Low. It's templated text + a version field in the existing `generate-guide` pipeline. The
intellectual work is rule selection and honest framing, not engineering.

---

## FEATURE 16 — Response cache with watcher invalidation

ZSTD-compressed, 24h-TTL cache inside the DB, keyed by `(tool, normalized_args, db_mtime
bucket)`; invalidated when a file under the query scope changes (10s tick to absorb
editor save-storms); LRU eviction at a size cap. Constraint: no daemon, agents are
short-lived → cache must be disk-resident.

### (1) Research foundation

- **RFC 9111 — HTTP Caching** — IETF, 2022. https://www.rfc-editor.org/rfc/rfc9111.html
  [EVIDENCE] Canonical semantics for the *bypass/default* model NestWeaver should mirror:
  freshness lifetime (TTL ≈ `max-age`), staleness, revalidation, and the rule that a stale
  response MUST NOT be reused without successful validation. The directive model (request can
  ask `no-cache`/`no-store` to bypass; response carries freshness) is the right mental model
  for a `--no-cache` flag and a default-on cache. Also: "a cache MUST ignore unrecognized
  cache directives" → forward-compatible cache metadata.

- **Zstandard (zstd)** — Facebook/Meta. https://github.com/facebook/zstd,
  https://engineering.fb.com/2016/08/31/core-infra/smaller-and-faster-data-compression-with-zstandard/
  [EVIDENCE] Configurable levels (negative/fast → 22/max); at default it matches zlib's ratio
  with much higher compress+decompress speed; **decompression speed is roughly constant
  across levels** (LZ-family property) — so a higher compression level costs write time but
  not read time, which is ideal for a write-once/read-many response cache. A **trained
  dictionary** mode exists for many-small-records workloads (cache entries are small,
  similar JSON blobs → a shared dictionary materially improves ratio). Cite for the ZSTD choice.

- **Build-system content-addressed invalidation (Bazel/Buck)** — prior art for scope-based
  invalidation. Bazel: https://queue.acm.org/detail.cfm?id=3287302,
  https://www.buildbuddy.io/blog/bazels-remote-caching-and-remote-execution-explained/.
  [EVIDENCE] Action cache (AC) + content-addressable storage (CAS): an action's result is
  keyed by a digest over its *immediate inputs*; Buck keys actions by digests of dependency
  actions (no incremental state needed). Direct analogy: NestWeaver should key a cached
  response by a digest over the *files in the query's scope* (its inputs), so any in-scope
  change changes the key — the build-systems' proven approach to scope-based invalidation.

- **SQLite-as-cache patterns** — [EVIDENCE-of-pattern] caching in SQLite is often faster than
  many small filesystem files (https://www.sqlite.org/fasterthanfs.html is the canonical
  reference for the "faster than the filesystem" claim); community libraries combine SQLite +
  LRU + TTL (e.g. https://github.com/jkelin/cache-sqlite-lru-ttl, https://pypi.org/project/disklru/).
  SQLite's *own* page cache uses LRU eviction. Validates a single-file, disk-resident cache
  with LRU+TTL inside the DB — exactly the daemon-less requirement.

### (2) Prior art / projects + tradeoffs

- **Bazel/Buck remote+disk cache** — content/input-digest keying, independent eviction of AC
  vs CAS, fallback to recompute on miss/error (https://queue.acm.org/detail.cfm?id=3287302).
  Tradeoff: their digests cover full transitive inputs; NestWeaver's "scope" is fuzzier (a
  query touches an unbounded neighbor set), so input-set computation is the hard part (see
  pitfalls).
- **HTTP `Cache-Control`/`ETag` (RFC 9111)** — bypass directives + validators. Tradeoff:
  full revalidation semantics are overkill for a local single-writer cache; borrow TTL +
  bypass + "don't serve stale unvalidated," skip conditional-request machinery.
- **zstd dictionary training** — strong for small homogeneous entries; tradeoff is dictionary
  lifecycle (must be regenerated as response shapes drift; version it).
- **The cache-invalidation aphorism** — "There are only two hard things in Computer Science:
  cache invalidation and naming things" (Phil Karlton; widely attributed,
  https://www.karlton.org/2017/12/naming-things-hard/). [EVIDENCE-of-attribution] Frames why
  the invalidation design (not the storage) is the real risk here.

### (3) Recommended approach for NestWeaver

- **Disk-resident is mandatory** (no daemon; agents are short-lived). Store the cache as a
  table/namespace inside the LadybugDB file (or a sibling sidecar like the existing
  `.pagerank.json`, `.summaries.json`, etc.), so a fresh short-lived process gets warm hits.
  This is the single most important correctness constraint and it rules out any in-memory-only
  scheme. [EVIDENCE: SQLite-as-cache patterns show disk-resident local caches are viable/fast.]
- **Key = `(tool, normalized_args, db_generation/db_mtime bucket)`**, plus a digest over the
  in-scope file set (Bazel/Buck input-digest analogy). NestWeaver already has a
  `graph_generation` counter (commit d9bc01d) and `<db>.filemeta.json` (per-file mtime/size/
  hash) — **reuse both**: `graph_generation` is a cheap coarse invalidator (any index change
  bumps it), and `filemeta` hashes give precise scope-level invalidation. Prefer hash-bucketed
  keys over raw mtime where possible (mtime granularity/clock-skew issues).
- **Args normalization** is essential for hit rate: canonicalize ordering, defaults, casing,
  path normalization before hashing (cache-key normalization is standard practice; getting it
  wrong = silent misses or, worse, collisions).
- **TTL = 24h** as an upper bound (RFC 9111 freshness model: never serve stale-past-TTL
  without revalidation — here "revalidation" = recompute on generation mismatch).
- **ZSTD at a mid level** (decompression is level-independent → favor ratio); consider a
  **trained dictionary** since entries are small, similar JSON. Version the dictionary.
- **Invalidation via the existing watcher** (commit e3d14be `--watch`, d9bc01d watcher
  shared-store): on file change, advance `graph_generation` and/or evict cache rows whose
  scope-digest includes the changed file; **batch on a ~10s tick** to absorb editor
  save-storms (debounced, matching the existing watch debounce design).
- **LRU + size cap**: evict least-recently-used entries when over the byte cap; track
  `last_accessed`. Combine with TTL (TTL = correctness bound, LRU = space bound), mirroring
  `cache-sqlite-lru-ttl`.
- **Bypass**: `--no-cache` flag and write-skip for tools whose results must always be fresh,
  mirroring RFC 9111's request directives.

### (4) Pitfalls / failure modes + mitigations

- **Stale cache without a daemon** — the headline risk. A short-lived agent process won't see
  another process's mid-query writes, and there's no long-running invalidator. Mitigations:
  (a) make `graph_generation` part of the key so any reindex/`--watch` tick automatically
  misses stale entries; (b) check generation at read time and treat mismatch as a miss;
  (c) rely on the watcher to advance generation/evict on change. **Correctness rests on the
  key, not on a background process** — this is the design's load-bearing decision.
- **Scope-invalidation misses** (a query's result depends on a file not counted in its
  "scope," e.g. a transitively-imported file or a deleted symbol). [EVIDENCE: Bazel/Buck show
  you must capture the *full* input set, including transitive deps.] Mitigations: define scope
  conservatively (over-include neighbors actually traversed); fold `graph_generation` in as a
  coarse safety net so a missed precise dependency still gets caught by any global index bump;
  prefer false-evict over false-hit.
- **mtime granularity / clock skew / mtime-unchanged edits** (same-size, same-second writes).
  Mitigation: use `filemeta.json` content hashes, not bare mtime, for the scope digest;
  "mtime bucket" only as a coarse cheap pre-filter.
- **Save-storm thrash** (editor autosave → constant eviction). Mitigation: 10s debounce tick
  (as specified); coalesce.
- **Compression CPU on the write path.** Mitigation: zstd's level/speed knob; only cache
  responses above a size threshold; async/lazy compression if needed.
- **Cache poisoning via key collision** from bad normalization. Mitigation: include a schema/
  version byte in the key; collision-resistant hash over fully-normalized args; unit tests on
  normalization.
- **Unbounded growth.** Mitigation: hard byte cap + LRU; periodic TTL sweep.

### (5) Complexity / effort

High — and the highest-risk of the four. Storage/compression/LRU/TTL are routine; **correct
scope computation + daemon-less invalidation are the hard, bug-prone parts** ("cache
invalidation" aphorism applies literally). Strongly leverage existing infra
(`graph_generation`, `filemeta.json`, the `--watch` watcher) rather than building new
change-detection. Build the disk-resident state layer once and share it with Feature 10's
`bundle_id` store.

---

## Cross-feature notes

- Features 10 and 16 share a **disk-resident state/cache layer** keyed on
  `graph_generation` — build once.
- Features 14 and 15 share a **single-source, versioned guidance** concern; 14 is the dynamic
  (hook) channel, 15 is the static (generated-guide) channel. Same content source, two adapters.
- *Lost in the Middle* (token budgeting / front-loading) is the connective tissue across 10
  and 15: front-of-context space is scarce and high-leverage, so both bundle output and hard
  rules must be tight and ranked.
- Honest framing for the RFC: guidance/rule features (14, 15) are **evidence-backed as
  helpful but probabilistic** (CoVe + Instruction Hierarchy support them; Geng et al. 2025 and
  2308.10819 show they are not guaranteed-obeyed). Retrieval/cache features (10, 16) rest on
  firmer, more deterministic ground.

## Source index (all retrieved May 2026)
- Lewis et al. 2020, RAG — https://arxiv.org/abs/2005.11401
- Liu et al. 2023/2024, Lost in the Middle — https://arxiv.org/abs/2307.03172 · https://aclanthology.org/2024.tacl-1.9/
- Gao et al. 2023, HyDE — https://arxiv.org/abs/2212.10496 · https://aclanthology.org/2023.acl-long.99/
- Singh et al. 2025, Agentic RAG survey — https://arxiv.org/html/2501.09136v4 · https://github.com/asinghcsu/AgenticRAG-Survey
- MA-RAG 2025 — https://arxiv.org/abs/2505.20096
- MCP tools spec 2025-06-18 — https://modelcontextprotocol.io/specification/2025-06-18/server/tools
- MCP pagination — https://modelcontextprotocol.io/specification/2025-06-18/server/utilities/pagination
- Aider repo-map — https://aider.chat/docs/repomap.html
- Microsoft GraphRAG — https://github.com/microsoft/graphrag
- Claude Code hooks reference — https://code.claude.com/docs/en/hooks
- claude-code-hooks-mastery — https://github.com/disler/claude-code-hooks-mastery
- Wallace et al. 2024, Instruction Hierarchy — https://arxiv.org/abs/2404.13208 · https://openai.com/index/the-instruction-hierarchy/
- Geng et al. 2025, Control Illusion (IH failure) — https://arxiv.org/abs/2502.15851
- Instruction-Following Robustness to Prompt Injection 2023 — https://arxiv.org/pdf/2308.10819
- Dhuliawala et al. 2024, Chain-of-Verification — https://arxiv.org/abs/2309.11495 · https://aclanthology.org/2024.findings-acl.212/
- NeMo Guardrails — https://github.com/NVIDIA-NeMo/Guardrails · https://docs.nvidia.com/nemo/guardrails/
- Guardrails AI — https://guardrailsai.com
- OWASP LLM Prompt Injection Prevention — https://cheatsheetseries.owasp.org/cheatsheets/LLM_Prompt_Injection_Prevention_Cheat_Sheet.html
- RFC 9111 HTTP Caching — https://www.rfc-editor.org/rfc/rfc9111.html
- Zstandard — https://github.com/facebook/zstd · https://engineering.fb.com/2016/08/31/core-infra/smaller-and-faster-data-compression-with-zstandard/
- Bazel remote cache (ACM Queue) — https://queue.acm.org/detail.cfm?id=3287302
- BuildBuddy on Bazel caching — https://www.buildbuddy.io/blog/bazels-remote-caching-and-remote-execution-explained/
- SQLite faster-than-filesystem — https://www.sqlite.org/fasterthanfs.html
- cache-sqlite-lru-ttl — https://github.com/jkelin/cache-sqlite-lru-ttl
- "Naming things / cache invalidation" (Karlton) — https://www.karlton.org/2017/12/naming-things-hard/
