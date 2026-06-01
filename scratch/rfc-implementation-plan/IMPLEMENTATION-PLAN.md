---
title: NestWeaver — Research-Backed Implementation Plan (RFC v0.9.1+ feature track)
status: draft
created: 2026-05-29
relates_to:
  - ~/Desktop/rfc-v0.9.1-quality-and-feedback.md (source RFC)
  - scratch/rfc-implementation-plan/research/*.md (7 cited research dossiers)
covers: Features 1–17 (Bugs #12 and #19 already landed on feat/ui-next-gen-r3f)
---

# NestWeaver — Research-Backed Implementation Plan

This plan turns the RFC's feature asks into an engineering roadmap, each backed by
primary research, prior-art systems, and the **actual NestWeaver codebase**. Every
non-obvious claim traces to one of the seven research dossiers in `research/`, which
hold the full citations with retrieved URLs.

> **Sourcing discipline.** The research agents were required to cite only sources they
> actually retrieved, with URLs, and to mark anything unverifiable `[UNVERIFIED]`.
> Vendor/blog performance numbers are labelled as such and are **not** treated as
> peer-reviewed fact. Where a number gates a decision (nDCG deltas, test-reduction %),
> we validate on our own corpus rather than importing it.

---

## 0. Status

- **Bug #12** (project_context drops notes) — **done** on `feat/ui-next-gen-r3f`. The
  RFC's Ask-1 alone was insufficient (seeded notes land in `seeds`, disjoint from the
  rendered `connected`); the shipped fix is two-part (seed notes **+**
  `promote_member_notes_into_connected`). TDD'd + integration-guarded.
- **Bug #19** (`--config` on brain commands) — **done**. The flag was already present
  (RFC repro tested hyphenated command names that don't exist); the real residual was an
  inert flag. Added `InstanceConfig.db` + `resolve_db_with_config` so `--config` selects
  the DB (precedence: `--db` > config `db` > `NESTWEAVER_DB`/default).
- **17 features remain.** This document plans all of them.

### Research dossiers (full citations + URLs)
| File | Covers |
|------|--------|
| `research/ranking-priors-query-expansion.md` | F6, F7 |
| `research/ltr-reranking-feedback.md` | F1 (ranking side), F17 |
| `research/code-search-trigram-symbol-reads.md` | F3, F4, F5, F8 |
| `research/api-contract-graphs.md` | F2 |
| `research/temporal-rank-test-selection.md` | F12, F13 |
| `research/agent-memory-knowledge-base.md` | F1 (memory side), F11, F9 |
| `research/agentic-orchestration-guardrails-cache.md` | F10, F14, F15, F16 |

---

## 1. Corrections the research surfaced — read before planning sprints

These change scope or spec and should be reflected back into the RFC.

1. **F5 premise is wrong: `Symbol` has no `end_line`.** `crates/nestweaver-schema/src/nodes.rs`
   `Symbol` stores `start_line: u32` only. Every "symbol body" capability (F5, the Symbol
   path of F3, and the Symbol path of F8) needs `end_line` added — a schema-version bump +
   re-index. Tree-sitter already yields the end span at parse time, so the cost is plumbing,
   not analysis. **This is a Phase-0 enabler, not a per-feature task.** (`Section` and
   `Heading` already carry `start_line`+`end_line`, and `Section` carries full `text`, so the
   *note* paths of F5/F8 are essentially free.)

2. **F6 must apply the prior *after* RRF, or it no-ops.** RRF (Cormack, Clarke, Büttcher,
   SIGIR 2009) is rank-only — it discards raw scores. Multiplying a path prior into BM25/PPR
   scores *before* fusion only matters if it reorders the pre-fusion list. The clean,
   fusion-independent design is a single post-fusion multiply on `node.relevance`, clamping the
   **final product** (not each glob) to `[0.05, 5.0]`. This is exactly how Elasticsearch
   `function_score` (`boost_mode: multiply`, `max_boost`) and Vespa do it.

3. **F7's discounted weights don't survive fusion either.** Same root cause: PRF term weights
   (0.3×) and alias weights (0.5×) change the *BM25-internal ordering* and reach the final
   ranking only via changed BM25 ranks. Set expectations accordingly; don't promise the weights
   appear in the fused score.

4. **F12's clamp can never bind as written.** With `git_activity_score ∈ [0,1]` and
   `activity_weight = 0.6`, `(1 + 0.6·(score − 0.5))` ranges only over **[0.7, 1.3]** — the
   proposed `[0.4, 1.6]` clamp is unreachable. Either raise `activity_weight`'s max or tighten
   the clamp. (Temporal PageRank, Rozenshtein & Gionis, ECML-PKDD 2016, also says: apply decay
   as a **post-hoc re-rank multiplier**, not inside the PageRank fixpoint, so the walk still
   converges to static PageRank under a flat signal.)

5. **F1/F17 feedback-loop bias is the headline risk, formally.** Ensign et al., "Runaway
   Feedback Loops in Predictive Policing" (FAT* 2018) proves self-collected feedback compounds
   bias. The RFC's 2.0× cap bounds magnitude but not direction. The plan adds a **uniform
   exploration floor** (Ensign's structural fix) and a **negative signal** (reformulation =
   bad), and makes both features run against a held-out benchmark, not their own logs.

6. **`serde_yaml` 0.9 (current dep) is deprecated/unmaintained.** F2 adds OpenAPI YAML parsing;
   migrate new contract YAML to `serde_yaml_ng` 0.10 or `saphyr`.

7. **Token-savings figures for F5/F8 (30–60%, 10–50×) are vendor/blog only.** No peer-reviewed
   measurement was found. Ship our own before/after instrumentation; don't cite the percentages
   as fact.

---

## 2. Cross-cutting foundations (build once; many features depend)

The single biggest efficiency in this track is recognizing that **seven of the seventeen
features share five substrates.** Build these first and the per-feature work shrinks.

### 2.1 — `Symbol.end_line` (enabler for F3·F5·F8)
Add `end_line: u32` to `Symbol` (schema), populate from the tree-sitter node end during
parsing (`nestweaver-parser`), persist (`nestweaver-store` write + `db.rs` table), bump the
schema version, re-index. **Blocks:** F5, F3-symbol, F8-symbol.

### 2.2 — Git-mining substrate (enabler for F12·F13)
One module (`nestweaver-engine` or a new `git_activity.rs`) that walks `git log --name-only`
per repo and exposes: per-file touch history, **author date** (not commit date — rebases/squash
distort commit date), merge-commit detection, and **bulk-commit filtering as a per-repo
percentile** (commit sizes are heavy-tailed; a fixed ≥500 cutoff is wrong for small repos —
Hindle, MSR 2008, on large-commit/perfective skew). **Feeds:** F12 (recency multiplier), F13
(co-change as a fallback test signal). Reuse the existing `<db>.filemeta.json` change-detection.

### 2.3 — Disk-resident state layer keyed on `graph_generation` (enabler for F10·F16)
There is no daemon and agents are short-lived, so shared state must be on disk. Commit `d9bc01d`
added a **`graph_generation` counter** — but **verification (see ADDENDUM §B) found it is
in-memory only, reset to 0 on every open, and bumped solely by the watcher loops, never by the
one-shot `index` command.** As-is it **cannot** serve as a cross-process invalidation key: every
short-lived agent sees `gen=0`. To use it, it must be **persisted to a sidecar and bumped on
`index`** (a small Phase-0 enabler). Otherwise the cross-process cache premise (F16) does not
hold — and the adversarial review (ADDENDUM §C) recommends dropping F16 to a measured experiment
for exactly this reason. Combined with `<db>.filemeta.json` content hashes, a *persisted*
generation would give both a coarse and a precise invalidation key. Build one small disk-resident
store (in the `.lbug` or a sidecar) used by **F16** (response cache) and **F10** (bundle TTL
state). Cache/bundle key = `(tool, normalized_args, graph_generation, scope-digest)`. Prior art:
Bazel/Buck **content-addressed action cache** (ACM Queue 3287302) — key by a digest over the
query's input file set; prefer content hashes over bare mtime; **correctness rests on the key
(generation mismatch at read = miss), never on a background sweep.**

### 2.4 — Interaction success-signal labeler (enabler for F1·F17)
F1 *records* labelled events; F17 *trains* on them. Build the labeler once.
`load_interaction_scores` already exists and is loaded at MCP startup (`mcp/src/lib.rs:49`) but
**consumed nowhere** — that's the gap. Event schema `{session_id, ts, kind, target, tool}` with
kinds `Query | Access | Impact | Followup | TerminalSuccess`. **Safe success signal** (grounded
in Fox et al., 2005, "Evaluating implicit measures…", where exit-type/session-end predicts
satisfaction): POSITIVE = the retrieved symbol/note is then *edited/written* (conversion-grade,
a signal web search lacks) or a clean session-end with no re-query; NEGATIVE = next action is
another search/reformulation; WEAK/zero = bare access (most position-biased — Craswell et al.
2008). Decay via MemoryBank's reinforce-on-access model `R = e^(−Δt/S)` (Zhong et al., AAAI 2024).

### 2.5 — Trigram index (enabler for F3·F4)
F4 (`count_patterns`) is "`rg --count` on F3's prefilter." Build the trigram posting index once;
F4 reuses it. Design in §3-F3.

### 2.6 — Single-source guidance (enabler for F14·F15)
F14 (dynamic PreToolUse hook) and F15 (static generated-guide rules) should read **one** rule
store, surfaced two ways. Build the rule store + versioning once.

### 2.7 — Evaluation harness (gate for F1·F7·F12·F17)
Multiple features promise ranking-quality gains that the literature says are **corpus-dependent
and sometimes negative** (PRF query drift; reranker over-expectations). Stand up a small
nDCG@10 / precision@k harness over NestWeaver's *own* code+notes corpus, with **time/query-based
cross-validation** (not random shuffle) and per-query win/loss + confidence intervals. nDCG:
Järvelin & Kekäläinen, TOIS 2002. This harness is the empirical contract that gates the
quality features — especially F17's "ship only if ≥5% nDCG@10."

---

## 3. Feature plans

Format per feature: **Ask** · **Research foundation** (key cites; full URLs in dossiers) ·
**Approach** (codebase-grounded) · **Key decisions** · **Pitfalls** · **Effort** · **Depends on**.

### Tier 1 — quick wins

#### F3 — Trigram-accelerated regex search
- **Ask.** `regex_search(pattern, [path_prefix], [kinds], [limit])` MCP tool + CLI; Rust `regex`
  over bodies with a trigram pre-filter; fall back to scan when no literals; budget + timeout.
- **Research.** Russ Cox, "Regular Expression Matching with a Trigram Index" (2012,
  swtch.com/~rsc/regexp/regexp4.html): trigram postings + compiling a regex into an AND/OR
  trigram query via emptyable/exact/prefix/suffix analysis; ~18% index-size overhead, ~100×
  candidate reduction; case-insensitive is much weaker. Prior art: Sourcegraph **Zoekt**
  (positional, ~1.2× RAM/3× index — heavier), **livegrep** (suffix arrays), GitHub Blackbird
  (sparse-grams). `regex` is already a dependency; `regex-syntax`'s literal `Extractor` is
  transitively present.
- **Approach.** New `nestweaver-store/src/regex.rs`. **Doc-ID-only postings** (Cox/codesearch
  style — smallest, most incrementally-updatable index; right for solo-dev scale + live
  re-indexing via `--watch`), *not* Zoekt's positional index. Use
  `regex_syntax::hir::literal::Extractor` (check `Seq::is_finite()`, exact vs inexact) for query
  planning instead of hand-rolling Cox's analyzer. New table
  `trigram_postings { trigram, node_uid }`; `--with-trigrams` opt-in flag on `index` until size
  is measured on the live DB. Budget: candidate-set hard cap (5000) + verification timeout (2s,
  `--max-millis`); fall back to full scan on infinite `Seq`.
- **Key decisions.** Doc-ID postings over positional; `regex-syntax` Extractor over custom AST
  walk; opt-in index until size validated.
- **Pitfalls.** Index bloat (measure on the 231 MB DB first; opt-in); no-literal patterns →
  scan-all (cap + timeout); case-insensitive weakness (document); unicode.
- **Effort.** Medium. **Depends on:** §2.5; `Symbol.end_line` only for the symbol-text path
  (note/section text already available).

#### F4 — `count_patterns`
- **Ask.** Counts-only companion: `{pattern, total_matches, files_matched, top_files[]}`,
  multi-pattern in one call.
- **Research.** No distinct literature — it's `rg --count`/`--count-matches` semantics over F3's
  prefilter.
- **Approach.** Reuse F3's candidate filter; run `regex` in count mode (no materialization);
  multi-pattern = union candidate sets, scan each doc once, attribute per pattern.
- **Effort.** Low (given F3). **Depends on:** F3.

#### F5 — Symbol-window reads (`read_symbols`)
- **Ask.** Return a symbol's source span; optional comment stripping; optional N neighbors; FQN
  resolution; token-budget aware.
- **Research.** Aider's tree-sitter **repo map** (2023): emit signatures/key lines, not whole
  bodies, under a token budget — the strongest non-vendor support for symbol-scoped context.
  Tree-sitter is the right precision tool (ctags imprecise; LSP needs a daemon → violates the
  no-daemon constraint). Token-savings %s are `[VENDOR/BLOG]` — instrument our own.
- **Approach.** New MCP tool + CLI; read the span via stored `start_line..end_line` from disk.
  Comment stripping in new `nestweaver-parser/src/strip.rs`: tree-sitter `comment` nodes (safe)
  for grammar languages; conservative whole-line-prefix strip for the regex languages;
  **default OFF** (false elision of shebangs, `#`/`//` inside strings/URLs, etc. is the named
  risk). FQN resolution via the existing symbol index; ambiguous → return all with
  `disambiguate: true`.
- **Pitfalls.** **Blocked on `Symbol.end_line` (§2.1).** Comment-strip false elision → off by
  default, tree-sitter-precise where possible.
- **Effort.** Medium. **Depends on:** §2.1.

#### F6 — Per-path `dampen`/`boost` ranking priors
- **Ask.** Continuous path-glob priors applied at PPR-output time, clamp `[0.05, 5.0]`,
  last-match-wins; `ranking explain` dry-run.
- **Research.** Robertson & Zaragoza, "The Probabilistic Relevance Framework: BM25 and Beyond"
  (FnTIR 2009): query-independent features enter as **document priors** — additive in log-space
  = **multiplicative on the relevance scale** (formal justification). BM25F caveat (Robertson
  et al., CIKM 2004) is about combining per-field *term* scores and does **not** apply to a
  whole-document path prior. Every production engine implements this multiplicatively **with a
  clamp** (ES `function_score`/`max_boost`, Vespa, Tantivy `Bm25Weight::boost_by`).
- **Approach.** `[ranking]` section in `InstanceConfig`/per-repo `.nestweaver.toml`; globs via
  `globset` (already transitive via `ignore`). **Apply post-fusion** on `node.relevance` in the
  final assembly step of `nestweaver-store/src/ranking.rs`, clamping the **product**. Resolve
  path→prior once at index/open into a sidecar (like `.pagerank.json`), not per query.
  `nestweaver ranking explain <uid>` prints base/matched-rules/final.
- **Pitfalls.** Applying before RRF (no-op — §1.2); compounding past the clamp (clamp product);
  dampen-to-zero hiding best matches (the 0.05 floor; dampen ≠ exclude).
- **Effort.** Low. **Depends on:** none (pure ranking-policy knob).

#### F7 — BM25 PRF + taxonomy synonym expansion
- **Ask.** Two-pass PRF (mine high-IDF terms from top-K, append at ~0.3×) + query-time alias
  expansion (~0.5×); cap query length; `--prf`, off by default.
- **Research.** Rocchio (1971, via Manning/Raghavan/Schütze *IIR* §9.1.1): down-weighting added
  terms is textbook (α=1, β=0.75, γ≈0.15; the RFC's 0.3× is *intentionally* more conservative
  for unjudged terms — defensible). RM3 operational defaults (Lavrenko & Croft 2001; confirmed
  from Indri/Anserini/pyserini `set_rm3(10,10,0.5)`): **K=10 feedback docs, N=10 terms, λ=0.5**.
  Thesaurus expansion (IIR §9.2.2): weight added terms less; multi-token synonyms must expand at
  **query time** (Lucene `SynonymGraphFilter`), normalized through the same analyzer,
  case-insensitive, excluding terms already in the query (Xapian `ExpandDecider`).
- **Approach.** Two-pass bag-of-words PRF where BM25 actually lives — `tantivy_index.rs` +
  the fusion point `query.rs::rrf_fuse` (~`query.rs:1001`); there is no `bm25.rs` (no full RM3 LM
  needed): K=10, N=10, original 1.0, PRF 0.3×, aliases 0.5×; rank candidate terms by IDF
  (Tantivy `Bm25StatisticsProvider`); cap total query length (64). Aliases from
  `_brain/taxonomy.md`, memoised by mtime (`nestweaver-engine/src/taxonomy.rs`). Surface
  `expansion_terms`/`expansion_aliases` in `--debug`.
- **Pitfalls.** **Query drift** (the classic PRF failure) — down-weight, cap N, prefer high-IDF,
  consider **selective expansion** gated on the pass-1 score gap; per-query inconsistency (helps
  average MAP, hurts some queries → keep opt-in); analyzer mismatch silently dropping aliases.
- **Effort.** Medium (aliases Low). **Depends on:** §2.7 (to validate it actually helps *our*
  corpus). Reported deltas are directional only (+10–30% MAP on weak TREC baselines) — **do not
  expect TREC magnitudes; gate on our harness.**

#### F8 — Tiered display (inline body for high-confidence hits)
- **Ask.** Inline body when normalized relevance ≥ 0.75; token-budget aware; off by default.
- **Research.** Motivated by "Lost in the Middle" (Liu et al., TACL 2024) — front-load
  high-value content; saves a round-trip. Zoekt shows a defensible normalized score exists.
- **Approach.** `inline_body: Option<String>` on the connected-node shape; populated only when
  `relevance ≥ threshold` and the caller opts in; bodies count against `token_budget` ahead of
  metadata; `[response] inline_body_threshold/inline_max_body_tokens`.
- **Key decision.** Scores **must be normalized + gap-checked** or 0.75 is meaningless on a raw
  hybrid score.
- **Pitfalls.** Unnormalized threshold; response-size jumps (off by default).
- **Effort.** Low. **Depends on:** F5 (Symbol bodies) — note/section bodies already available.

#### F9 — Promote document-graph tools to `brain.*`
- **Ask.** `brain_broken_links`, `brain_orphan_documents`, `brain_topic_clusters`,
  `brain_tag_graph`, `brain_doc_stats`; `/audit-vault` becomes a thin orchestrator.
- **Research.** Orphan detection value: Wikipedia orphan study (arXiv:2306.03940) — ~14.7%
  orphaned, de-orphaning → +6.5% pageviews; **index/MOC notes are false-positive risk**.
  Clustering: **Leiden** (Traag, Waltman, van Eck, Sci. Rep. 2019) guarantees connected
  communities (Louvain leaves up to 16% disconnected). Link suggestion: Adamic–Adar best
  topological predictor (Liben-Nowell & Kleinberg, CIKM 2003). KG maintenance framing: Paulheim,
  "Knowledge Graph Refinement: A Survey" (2017).
- **Approach.** These mostly **reuse existing infrastructure** — wikilink edges, tag nodes, the
  code-side `clusters` (run Leiden over the wikilink subgraph), the resolver + aliases for
  broken-link suggestions. MOC/index allowlist to suppress orphan false-positives.
- **Effort.** Low–Medium. **Depends on:** none (data already in the graph).

### Tier 2 — feedback, composites, agent-efficiency

#### F1 — Agent feedback loop
- **Ask.** Consume interaction scores as a capped (2.0×) multiplicative boost on PPR
  personalization; distinguish success from access; new event kinds; CLI to inspect.
- **Research.** Topic-Sensitive PageRank (Haveliwala, WWW 2002) — the personalization/teleport
  vector is the principled place to inject the boost; renormalize after. Implicit feedback &
  bias: Craswell et al. 2008 (position bias), Fox et al. 2005 (session-end predicts
  satisfaction), Joachims et al. 2017 (position-discounted/IPS), **Ensign et al. FAT* 2018
  (runaway feedback loops — the core danger)**.
- **Approach.** Implement §2.4's labeler; load the sidecar once per call (path already plumbed)
  and apply the capped boost on the teleport step in `ranking.rs`. Add **negative signal**
  (reformulation) and a **uniform exploration floor** so low-scored nodes still surface. CLI
  `interactions show --uid`.
- **Pitfalls.** Feedback-loop entrenchment (cap + decay + exploration floor + negative signal);
  TerminalSuccess mis-attribution (session walked away ≠ success — weight conservatively);
  query-text privacy/reproducibility (machine-local scores make benchmarks non-deterministic —
  document, and benchmark with feedback disabled).
- **Effort.** Low–Medium (infra exists). **Depends on:** §2.4, §2.7. **The deliverable is
  risk-reduction, not a quality number** — no transferable single-user metric exists.

#### F2 — Framework-aware contract linking (OpenAPI/gRPC/GraphQL)
- **Ask.** Parse contracts → `Contract` nodes; `IMPLEMENTS_CONTRACT` / `CONSUMES_CONTRACT`
  edges across repos with confidence scoring; wire into `cross_repo_contracts`.
- **Research.** Consumer-Driven Contracts (Robinson/Fowler, 2006) maps directly to the
  node+edge model; Pact / Spring Cloud Contract operationalize it (candidate high-confidence
  ingestion later). Specs: OpenAPI 3.1, Protobuf/gRPC (`package.Service/Method` FQN — no
  templating), GraphQL. **Load-bearing reality check:** Schneider et al., "Comparison of Static
  Analysis Architecture Recovery Tools" (2024, arXiv:2412.08352 / EMSE) — purpose-built tools
  cap at **~F1 0.79** on REST endpoint extraction. Code2DFD (Schneider & Scandariato, JSS 2023)
  validates keyword/signature detection. Backstage Software Catalog is the closest model
  (typed `kind: API`, `providesApis`/`consumesApis`) but **human-authored** — our value-add is
  *deriving* those edges from code. OpenTelemetry service graph = runtime topology (complementary,
  not a competitor: we see all code-declared paths pre-deploy, no instrumentation, at the ≤0.79
  F1 cost).
- **Crates (verified on crates.io/docs.rs 2026-05-29).** OpenAPI: **`openapiv3` 2.2.0** (OAS 3.0)
  + **`oas3` 0.22.0** (OAS 3.1) to cover both. gRPC: **`protox` 0.9.1** (pure-Rust, no `protoc`)
  — preferred over `protobuf-parse` (explicitly "not for direct use"). GraphQL: **`apollo-parser`
  0.8.6**. Avoid `protobuf-parser` (abandoned 2018) and `swagger` (codegen, not a parser).
  Migrate off deprecated `serde_yaml` for YAML.
- **Approach.** Two node producers (spec files = authoritative 1.0; code-derived handlers mint
  nodes when no spec), merged by UID. UID scheme `contract:http:POST:/v1/approvals`,
  `contract:grpc:approvals.v1.Approvals/Create`, `contract:graphql:Mutation.createApproval`.
  **Path normalization is load-bearing** — collapse `:id`/`{id}`/`${id}`/`<id>`, ignore slot
  names, resolve base/mount paths, identically on both sides. Handler detection via tree-sitter
  (java/ts/go grammars already deps): Spring annotations (highest precision), NestJS decorators,
  Express literals, Go chi/gorilla. Caller detection v1 = TS `fetch`/axios literals + typed
  clients via `operationId`. Confidence 1.0/0.8/0.5 on the edge; impact queries filter by min
  confidence.
- **Pitfalls.** Templating mismatch (canonical normalization); **base-path/mount resolution =
  biggest recall killer** (thread mount context, fall back to 0.5); false cross-repo matches on
  generic paths (`/health`, `/login`) → denylist + prefer intra-feature matches + surface
  ambiguity (exit-code-3 convention); generated-client noise; GraphQL consumers (opaque query
  strings) → **defer to v2**.
- **Effort.** gRPC nodes Low; OpenAPI nodes Low–Med; Spring/NestJS handlers Med; Express/Go
  Med–High; TS consumers High; GraphQL consumers v2. **Highest product value in the RFC** — this
  is the one capability name-matching fundamentally can't do.
- **Depends on:** none structural (no new grammars). Pull forward.

#### F10 — `investigate` bundle primitive
- **Ask.** One call returns an architectural map (clusters as domains, top-PageRank node as
  entry point) + `bundle_id` (24h TTL) + `investigate_expand`/`investigate_hydrate`.
- **Research.** RAG (Lewis et al., NeurIPS 2020); **"Lost in the Middle"** (Liu et al., TACL
  2024) justifies tiered/front-loaded bundling; Aider repo-map (PageRank-ranked, token-budgeted)
  and Microsoft **GraphRAG** (community-detection → cluster summaries) mirror the cluster→domains
  step. **MCP spec (verified):** tool results carry `content` + optional `structuredContent`;
  `resource_link` returns file URIs (ideal for entry points hydrated later); **cursor pagination
  is spec'd only for `tools/list`, not `tools/call`** → paging must be app-level `bundle_id` +
  page token, exactly as proposed.
- **Approach.** New `nestweaver-engine/src/investigate.rs`; compose existing primitives
  (PRF-on hybrid search → group → neighbors → Leiden clusters → top-3 domains + entry points);
  inline ≤5 high-confidence bodies (F8); bundle state in §2.3's disk layer, tagged with
  `graph_generation`.
- **Pitfalls.** Over-bundling recreates the mid-context problem (strict token budget,
  hydrate-on-demand); stale `bundle_id` within TTL (generation tag); cluster instability.
- **Effort.** Medium–High. **Depends on:** §2.3, F7 (PRF), F8 (inline), F9 (Leiden). Sequenced
  after Tier 1, as the RFC notes.

#### F11 — Memory-bank semantics over the vault
- **Ask.** Typed edges (Supersedes/DependsOn/CausedBy/RelatesTo); 7 health checks; tiered
  consolidation pipeline; `brain_memory_related`.
- **Research.** Standards map cleanly: **SKOS** (`broader`/`narrower`/`related`) and **PROV-O**
  (`wasDerivedFrom`, `wasInformedBy`, `wasRevisionOf`) → our typed relations. Promotion gates
  grounded in PKM: Ahrens (fleeting→literature→permanent), Matuschak evergreen (densely-linked,
  ≥3 inbound), Forte PARA; survival window ≥14d echoes MemoryBank/Ebbinghaus. **A-MEM** (Xu et
  al., NeurIPS 2025) caution: its LLM "memory evolution" rewrites neighbors — a provenance risk;
  therefore **suggest + record `wasDerivedFrom`, never silently mutate.** Paulheim 2017 frames
  the 7 checks as KG error-detection + completion.
- **Approach.** Deterministic typed-edge derivation from frontmatter/section names (no LLM); 7
  checks as graph traversals (orphans/broken-links reuse F9); consolidation **dry-run by
  default**, emitting a manifest the user accepts.
- **Pitfalls.** Orphan/stale false-positives for MOC/convention notes (tier-aware allowlist);
  false consolidation (require temporal spread + link threshold); never auto-mutate.
- **Effort.** Medium–High (consolidation is the long pole). **Depends on:** F9.

#### F12 — Git-activity-dampened CodeRank
- **Ask.** Per-file recency multiplier on PageRank at compute time.
- **Research.** Temporal PageRank (Rozenshtein & Gionis, ECML-PKDD 2016 — degrade-to-baseline
  under flat signal); TimedPageRank (Yu/Li/Liu, WI 2005 — exponential age decay consistently
  improved retrieval); **Nagappan & Ball, ICSE 2005** (relative/normalized churn predicts
  defects, absolute churn doesn't → normalize); Hassan ICSE 2009 (change entropy — future
  refinement); Lewis et al. ICSE 2013 (a valid Google bug-predictor changed *no behavior* when
  unexplained → keep it explainable + bounded).
- **Approach.** Keep `exp(-Δdays/180)`; apply as a **post-hoc multiplier, not in the PageRank
  fixpoint**; center at 0.5. **Fix the clamp/weight inconsistency (§1.4).** Per-repo opt-out for
  squash-only repos. Use **author date**; bulk filter as per-repo percentile (§2.2).
- **Pitfalls.** churn≠quality (frame as *retrieval recency*, not health); timestamp distortion
  (author date, detect merges); cross-repo cadence skew (normalize within-repo).
- **Effort.** Low–Medium. **Depends on:** §2.2. Quality gain is `[UNVERIFIED/novel]` → A/B on
  §2.7.

#### F13 — `affected_tests`
- **Ask.** changed files → changed symbols → reverse CALLS/IMPORTS (depth 3) → test files,
  tiered by priority.
- **Research.** Yoo & Harman survey (STVR 2012 — selection vs prioritization); Rothermel &
  Harrold (TOSEM 1997 — defines a **"safe" RTS**; static call-graph RTS is **not** safe, and
  depth-3 is an explicit unsafe truncation); **STARTS** (ASE 2017 — closest comparable: static,
  class-firewall, reverse-traversal to reachable tests; unsafe under reflection); **Ekstazi**
  (ICSE/ISSTA 2015 — dynamic file-level RTS, ~32% avg/~54% long-suite reduction, catches what
  static misses); Google TAP (ICSE-SEIP 2017); **Meta Predictive Test Selection** (ICSE-SEIP
  2019 — ~2× reduction at >95% failure recall using graph distance + file metadata + history;
  the realistic ceiling and the recall bar).
- **Approach.** Position honestly as **STARTS-like static RTS on the source call graph** (less
  sound than bytecode). Reuse `impact`'s reverse traversal + `--depth`. Tiers = graph distance
  (1 direct / 2 caller / 3 transitive); order within tier by edge confidence (CALLS=1.0 …
  ACCESSES=0.4). Test detection by filename + annotation/macro heuristics per language. Set an
  explicit **measured recall target vs run-all** (Meta's posture), not provable safety.
- **Pitfalls.** Static unsafety (reflection/DI/codegen/data-driven tests missed) → conservative
  fallback + periodic full run; depth-3 truncation → warn when tests exist beyond cap; flaky
  tests → deprioritize via history; large/squash MRs → recommend run-all past a change threshold;
  **"no path found" ≠ safe-to-skip.**
- **Effort.** Medium. **Depends on:** §2.2 (co-change fallback), existing `impact`/`pr-impact`.

#### F14 — Subagent PreToolUse hook for guidance injection
- **Ask.** Hook on Task/Agent that injects dynamic guidance via a CLI; `install-hook`; reverse-
  migrate `CLAUDE.md`/`AGENTS.md`.
- **Research (verified vs official hooks docs).** PreToolUse receives `tool_input`/`cwd` on
  stdin and emits `hookSpecificOutput.additionalContext`; `type: "command"` runs a CLI,
  `mcp_tool` reuses the running server; Task is matchable (`"matcher": "Task"`). **Counter-
  evidence:** "Control Illusion" (Geng et al. 2025, arXiv:2502.15851) — instruction hierarchies
  are *not* reliably obeyed; injected guidance helps but isn't enforcement.
- **Approach.** Small fast `nestweaver subagent-guidance` subcommand emitting `additionalContext`;
  prefer the **`mcp_tool` hook type** (reuse the running MCP server — avoids cold-CLI/DB cost per
  spawn since agents are short-lived); ship via `setup claude-code`.
- **Pitfalls.** Runtime lock-in (Claude-Code-specific schema); untrusted-content injection;
  guidance ≠ enforcement.
- **Effort.** Low–Medium. **Note:** this and F15 are the **scope-creep features** (agent-harness
  layer, not graph engine) — flagged in the original review; build only if the single-source
  guidance store (§2.6) earns its keep.

#### F15 — Hard-rule guidance in `generate-guide`
- **Ask.** Canonical `**HARD RULE:**` block at the top of generated guides; versioned.
- **Research.** **"Enumerate then verify" is directly evidence-backed** by Chain-of-Verification
  (Dhuliawala et al., ACL 2024 Findings, arXiv:2309.11495 — draft→verify reduces hallucination).
  Top/privileged placement supported by the Instruction Hierarchy (Wallace et al./OpenAI 2024,
  arXiv:2404.13208). But rules are *helpful, not guaranteed* (Geng et al. 2025) — frame as
  defense-in-depth.
- **Approach.** Const rule array in `nestweaver-engine/src/guide.rs` (`--rules-from` override);
  position at top; `rules_version` in frontmatter. Keep rules **few** (rule-bloat dilutes scarce
  front-of-context space — Lost in the Middle).
- **Effort.** Low. **Depends on:** §2.6.

#### F16 — Response cache with watcher invalidation
- **Ask.** ZSTD, 24h TTL, keyed by `(tool, normalized args, db_mtime bucket)`, scope-invalidated,
  LRU, `--no-cache`.
- **Research.** RFC 9111 (TTL/bypass/never-serve-stale-unvalidated); ZSTD (decompression speed
  is level-independent → favour higher ratio; trained-dictionary mode fits small homogeneous
  JSON); **Bazel/Buck content-addressed action cache** (ACM Queue 3287302 — key by a digest over
  the input file set; the proven scope-invalidation prior art).
- **Approach.** Disk-resident in §2.3's layer. Key = `(tool, normalized_args, graph_generation,
  scope-digest)` using `<db>.filemeta.json` **content hashes** (not bare mtime). 24h TTL + LRU
  byte cap + `--no-cache`. Invalidate via the existing `--watch` watcher with 10s debounce; but
  **correctness rests on the key (generation mismatch at read = miss), never on the sweep.**
- **Pitfalls.** Stale cache without a daemon (key-based correctness, not background process);
  scope-invalidation misses (capture transitive deps à la Bazel; fold in `graph_generation` as
  safety net; prefer false-evict over false-hit). **Validate the "two runtimes in 60s" premise
  with a measured hit-rate before building the full machinery** (carried over from the original
  review).
- **Effort.** **High — highest-risk of its cohort.** Storage is routine; correct
  scope-computation + daemon-less invalidation are the bug-prone core. **Depends on:** §2.3.

### Tier 3 — opt-in model dependency

#### F17 — Lightweight learned listwise reranker
- **Ask.** Re-score top-50 with a ~20K-param listwise model (candle), gated to ship only at
  ≥5% nDCG@10 over hybrid.
- **Research.** Retrieve-then-rerank is validated (Nogueira & Cho monoBERT 2019 — *architecture*
  only; its +27% MRR is a 110M-param result, not a magnitude to expect from 20K params; Vespa
  phased ranking; SBERT cross-encoder docs). A ~20K-param model on ~7 tabular features **is
  credible** (input ~10–20 dims, largely monotone). Listwise loss: ListMLE (Xia et al. 2008) or
  ListNet (Cao et al. 2007), optional LambdaRank |ΔnDCG| weighting (Burges 2010). **Also train an
  XGBoost/LightGBM LambdaMART baseline** — trees dominate tabular LTR (2010 Yahoo LTR Challenge);
  ship whichever clears the gate (export to ONNX, run via `candle-onnx`/`ort`). Offline-train /
  online-serve mirrors OpenSearch/Elasticsearch LTR.
- **Approach.** Features `[rank_position, bm25, ppr, node_kind_onehot, is_inline_body, age_days,
  matched_alias_count]`; graded labels from §2.4's success signal, **IPS/position-discounted** so
  the reranker doesn't just relearn first-stage order. Single-file SafeTensors blob; silent skip
  if missing/stale.
- **Gate (harden the RFC's).** ≥5% nDCG@10 with **time/query-based CV** (not random shuffle),
  per-query win/loss + confidence interval (a 5% mean on ~40 queries can be noise), secondary
  MRR, default-off flag.
- **Pitfalls.** Reranker relearning first-stage order (the `rank_position` feature re-imports
  bias — IPS); tiny-sample noise in the gate; over-expecting web-scale gains.
- **Effort.** Medium. **Depends on:** F1 (success labels), §2.7. **Ship dead-last, only if F7 +
  F1 don't already close the gap** — as the RFC says.

---

## 4. Dependency graph & phased roadmap

```
Phase 0 (enablers)        Phase 1 (Tier-1)         Phase 2                Phase 3 (composites)     Phase 4
─────────────────────     ──────────────────       ──────────────        ─────────────────────    ───────
§2.1 Symbol.end_line ───► F5 ──► F8 (symbol)        F1 (needs §2.4,§2.7)  F10 (F7,F8,F9,§2.3)      F17
§2.5 Trigram index  ───► F3 ──► F4                  F7 (needs §2.7)       F11 (F9)                 (gated:
§2.2 Git substrate  ────────────────────────────► F12, F13              F16 (§2.3)                 F7+F1
§2.3 Disk state layer ──────────────────────────────────────────────► (F10, F16)                  first)
§2.4 Success labeler ─────────────────────────► F1 ──────────────────────────────────────────► F17
§2.6 Guidance store ──────────────────────────────────────────────► F14, F15
§2.7 Eval harness ──────────────────────────► gates F7, F1, F12, F17
F6 (standalone) ──► Phase 1        F9 (standalone) ──► Phase 1        F2 (standalone) ──► pull forward
```

**Recommended sequencing (revised from the RFC):**

- **Step Zero — configure + dogfood (precondition, see ADDENDUM §A).** NestWeaver's MCP is not
  configured in the user's working repos, so the tool isn't in the agent loop and there is no
  usage evidence yet. `nestweaver setup claude-code`, dogfood it, and run the benchmark including
  the new **Journey 8 (vault Project orientation)**. Until this is done, nothing below can be
  prioritized on evidence.
- **Phase 0 — Enablers (do first, unglamorous, unblocks everything).** `Symbol.end_line`,
  git-mining substrate, disk-state layer (note: `graph_generation` must be persisted + bumped on
  `index` first — see §2.3), success-signal labeler, eval harness, trigram index, guidance store.
  ~2–3 weeks.
- **Phase 1 — Token-cost + ranking quick wins.** F3, F4, F5, F8, F6, F9. All Low–Medium, no
  model, mostly reuse. Highest ROI-per-effort. **Pull F2 (contract linking) into here too** — it's
  the RFC's highest *product* value and has no structural blockers; don't bury it at position 2
  of 17.
- **Phase 2 — Quality signals (gated by §2.7).** F7 (PRF), F1 (feedback), F12 (temporal). Each
  must clear the benchmark or stay off-by-default.
- **Phase 3 — Composites + agent-efficiency.** F10 (investigate), F13 (affected_tests), F11
  (memory-bank), F16 (cache — *after* measuring the hit-rate premise), F14/F15 (guidance — only
  if §2.6 proves its worth; these are agent-harness scope creep).
- **Phase 4 — F17 reranker**, only if F7 + F1 leave a measurable gap.

This reorders the RFC's release plan in three ways: (a) a real **Phase 0** because so much is
shared infra the RFC treats as per-feature; (b) **F2 pulled forward** for product value; (c)
quality features (F7/F1/F12/F17) explicitly **gated** on the eval harness rather than shipped on
faith.

---

## 5. Risk register (top, cross-track)

| Risk | Features | Mitigation |
|------|----------|-----------|
| Feedback-loop / popularity bias entrenches bad rankings | F1, F17 | Exploration floor (Ensign 2018), negative signal, decay, 2.0× cap, benchmark with feedback **off** |
| Quality features ship without real gain (PRF drift, reranker noise) | F7, F12, F17 | §2.7 eval harness with time-based CV + per-query CI; off-by-default; explicit ship-gates |
| Static contract/test extraction is unsound (≤0.79 F1; depth-3 unsafe) | F2, F13 | Confidence scoring + min-confidence filters; "no path ≠ safe-to-skip"; periodic full runs; frame as complementary to runtime/dynamic |
| Daemon-less cache serves stale results | F16 | Key-based correctness (generation mismatch = miss); content hashes; prefer false-evict; **measure hit-rate before building** |
| Storage/invalidation surface sprawl (231 MB DB + many sidecars) | F3, F16, F10, F12, F17 | Opt-in indexes; one shared §2.3 state layer; measure size on the live DB |
| Scope creep into agent-harness layer | F14, F15 | Build §2.6 once; treat as defense-in-depth, not enforcement (Geng 2025); revisit product identity |
| `Symbol.end_line` schema change forces a re-index | F5, F3, F8 | Bundle into Phase 0; tree-sitter already has the end span |

---

## 6. Validation strategy (the recurring theme)

The literature is consistent that ranking-quality interventions are **corpus-dependent and
sometimes negative**. So:

1. **Build §2.7 first.** nDCG@10 / precision@k on NestWeaver's own code+notes corpus.
2. **Time/query-based CV**, never random shuffle (interaction data is temporal).
3. **Per-query win/loss + confidence intervals** — a small mean delta on ~40 queries is noise.
4. **Off-by-default** for every quality feature until it clears its gate on *our* data.
5. **Instrument our own token-savings** for F5/F8 rather than citing vendor percentages.
6. **A/B with the signal disabled** for F1/F12 so machine-local non-determinism doesn't
   contaminate the baseline.

---

## 7. Where the evidence lives

Full citations (authors, year, venue, retrieved URLs) and the per-feature deep dives are in:

```
scratch/rfc-implementation-plan/research/
  ranking-priors-query-expansion.md        (F6, F7)
  ltr-reranking-feedback.md                (F1-ranking, F17)
  code-search-trigram-symbol-reads.md      (F3, F4, F5, F8)
  api-contract-graphs.md                   (F2)
  temporal-rank-test-selection.md          (F12, F13)
  agent-memory-knowledge-base.md           (F1-memory, F11, F9)
  agentic-orchestration-guardrails-cache.md(F10, F14, F15, F16)
```
