---
title: Build-ready spec — Wave 2 (F9 doc-graph tools + F2-core contract linking)
created: 2026-05-29
companion_to: BUILD-SPEC-phase0-wave1.md, IMPLEMENTATION-PLAN.md, ADDENDUM-evidence-and-findings.md
scope: F9 (low-risk reuse) + F2-core (the highest-value differentiator, scoped to the trustworthy
       lanes per ADDENDUM §C). Surface verified against real code (ADDENDUM §B). No build/defer/kill
       calls — the deferred F2 surface (general HTTP-literal cross-repo CONSUMES, GraphQL consumers)
       stays in scope as F2-v2, sequenced after the core proves out.
---

# Build Spec — Wave 2

Same format as Wave 1: **Goal · Verified surface · Schema/signatures · Tasks · Tests (TDD) ·
Acceptance · Effort · Deps**. Effort S ≈ ≤1 day, M ≈ 2–4 days, L ≈ 1–2 weeks.

Wave 2 has two independent tracks:
- **F9** — promote document-graph operations to `brain.*` tools. Low risk; mostly new Cypher over
  data already in the graph.
- **F2-core** — contract-mediated cross-repo linking, scoped to the lanes the evidence says are
  trustworthy (gRPC, spec nodes, same-repo handlers, drift). Highest product value.

---

## F9 — Document-graph tools (`brain.*`)

Shared surface note: today only `count_wikilink_edges` (`read.rs:828`) and `wikilink_sources_to_note`
(`read.rs:876`, returns `Vec<BacklinkRow>`) exist; broken-link/orphan/tag-cooccurrence queries are
**greenfield**. Tag edges `NOTE_TAGGED_WITH` / `SECTION_TAGGED_WITH` already exist (`ranking.rs:266`).
A configurable **MOC/index allowlist** (`Projects.md`, `_brain/index.md`, …) suppresses
false-positives across the orphan/stats tools.

### F9.1 — `brain_broken_links`
- **Goal.** Find wikilinks whose target doesn't resolve (or resolves with `confidence < 1.0`), with
  fuzzy suggestions.
- **Verified surface.** Wikilink edges created during markdown indexing (`index_md.rs`); no audit
  query exists. Resolver + aliases available for suggestions.
- **Schema/signatures.** MCP `brain_broken_links([vault?, path_prefix?])` →
  `[{source_uid, source_path, wikilink_text, suggested_target_uids:[...]}]`. CLI
  `nestweaver brain broken-links [--json]`.
- **Tasks.** (1) query unresolved/low-confidence wikilink edges (`MATCH (s)-[r:WIKILINK…]->()` where
  target missing or `r.confidence < 1.0`); (2) suggest targets by **Adamic–Adar / fuzzy title match**
  (Liben-Nowell & Kleinberg, CIKM 2003) against existing note titles + aliases.
- **Tests (TDD).** A note with `[[Nonexistent]]` surfaces; a resolved link doesn't; a near-miss
  title gets a ranked suggestion.
- **Acceptance.** Count matches what `/audit-vault` reports today (±0).
- **Effort.** M. **Deps.** none.

### F9.2 — `brain_orphan_documents`
- **Goal.** Notes with zero inbound **and** outbound wikilinks, minus the allowlist.
- **Schema/signatures.** MCP `brain_orphan_documents([vault?, path_prefix?])` → `[{uid, path}]`. CLI
  `nestweaver brain orphans`.
- **Tasks.** (1) `MATCH (n:Note)` with no incident wikilink edges in either direction; (2) exclude
  allowlist (MOC/index/registry notes — Wikipedia orphan study warns these are the false-positive
  class).
- **Tests.** A linked note is excluded; an island note surfaces; an allowlisted index note is
  excluded even if islanded.
- **Acceptance.** Stable list; allowlist honored.
- **Effort.** S. **Deps.** none.

### F9.3 — `brain_topic_clusters`
- **Goal.** Leiden communities over the **wikilink (note↔note) subgraph**; cluster id + members +
  label (highest-PageRank note title).
- **Verified surface.** `cluster_dispatch.rs` runs Leiden but **hardcoded to code Symbol edges**
  (8 edge types) via `load_code_symbols_and_edges` (`read.rs:~990`). Reusing the algorithm over
  wikilinks needs a new `load_note_edges` loader (~50 LOC).
- **Schema/signatures.** New `read.rs::load_note_edges() -> (nodes, edges)` over wikilink edges; feed
  the existing Leiden path. MCP `brain_topic_clusters([vault?])` → `[{cluster_id, members:[uid],
  label}]`. CLI `nestweaver brain topic-clusters`.
- **Tasks.** (1) `load_note_edges`; (2) parametrize the Leiden entrypoint by graph source (don't fork
  the algorithm); (3) label = top-PageRank member title.
- **Tests.** Two densely-linked note groups → two clusters; label is the hub note.
- **Acceptance.** Mirrors `clusters` shape, scoped to vault.
- **Effort.** M. **Deps.** none (Leiden exists).

### F9.4 — `brain_tag_graph`
- **Goal.** Tag co-occurrence: `{tag, count, co_occurring:[{tag, count}]}`.
- **Verified surface.** `NOTE_TAGGED_WITH` edges exist; no co-occurrence query.
- **Schema/signatures.** MCP `brain_tag_graph()` → as above. CLI `nestweaver brain tag-graph`.
- **Tasks.** (1) count per-tag note frequency; (2) co-occurrence = notes sharing two tags; weight by
  **PMI** to surface meaningful pairs (suppress mega-tag dominance).
- **Tests.** Two tags on the same notes co-occur; a singleton tag has empty co-occurrence.
- **Acceptance.** Used by `/audit-vault` to spot taxonomy drift.
- **Effort.** S–M. **Deps.** none.

### F9.5 — `brain_doc_stats`
- **Goal.** One-call health dashboard.
- **Schema/signatures.** MCP `brain_doc_stats()` → `{total_notes, total_wikilinks, broken_wikilinks,
  orphans, avg_outdegree, top_tags, notes_by_year}`. CLI `nestweaver brain doc-stats`.
- **Tasks.** Compose F9.1/F9.2/F9.4 + counts (`count_wikilink_edges` exists) + `notes_by_year` from
  `created_at`/`modified_at`.
- **Tests.** All 7 keys present; broken/orphans match F9.1/F9.2.
- **Acceptance.** RFC F9 acceptance (`keys` includes all 7).
- **Effort.** S. **Deps.** F9.1, F9.2, F9.4.

> **Skill refactor (out-of-engine):** once F9.1–F9.5 land, `/audit-vault` collapses to a thin
> orchestrator over them and gains parity on the other runtimes. Track separately from engine work.

---

## F2-core — Contract-mediated linking (trustworthy lanes)

Scoped per ADDENDUM §C: lead with the unambiguous lanes; **present every contract edge as a
confidence-scored hypothesis, never ground truth.** The existing `cross_repo_contracts` tool does
name/import matching (`tools.rs:1445`) and the real contract matcher
(`resolver/src/cross_repo.rs::find_cross_repo_links`) is **unwired dead code** — so this is
**greenfield**, not integration.

### F2.0 — Enabler: wire `detect_frameworks()`
- **Goal.** Populate `Symbol.framework_hint` (today always `None`) — prerequisite for handler
  detection and broadly useful.
- **Verified surface.** `detect_frameworks()` in `crates/nestweaver-parser/src/frameworks.rs`
  (detects Spring/Flask/Express/etc. by signature substring → `(usize, FrameworkHint)`) is
  **exported but never called**; `index.rs` sets `framework_hint: None` everywhere.
- **Tasks.** (1) call `detect_frameworks()` in the indexing pipeline where Symbols are built; (2)
  set `framework_hint`; (3) expose in `symbol --json`.
- **Tests.** A Spring `@RestController` class gets `framework_hint = {framework: spring, role: …}`.
- **Acceptance.** `framework_hint` populated on a fixture repo.
- **Effort.** S. **Deps.** none. **Blocks.** F2.2.

### F2.1 — Contract nodes from spec files
- **Goal.** Parse OpenAPI/proto/GraphQL → `Contract` nodes (authoritative, confidence 1.0).
- **Verified surface.** New node kind (mirror how `Project` nodes are added: struct + table +
  write/read). Crates verified on crates.io (2026-05-29): **`openapiv3` 2.2.0** (OAS 3.0) +
  **`oas3` 0.22.0** (OAS 3.1); **`protox` 0.9.1** (pure-Rust, no `protoc`); **`apollo-parser`
  0.8.6**. **Migrate YAML off deprecated `serde_yaml` → `serde_yaml_ng`.**
- **Schema/signatures.** `Contract { uid, kind: http|grpc|graphql, verb?, path?/method, operation_id?,
  repo_uid, source_path, confidence }`. UID scheme: `contract:http:POST:/v1/approvals`,
  `contract:grpc:approvals.v1.Approvals/Create`, `contract:graphql:Mutation.createApproval`.
  **Path normalization** (collapse `{id}`/`:id`/`${id}`/`<id>`, ignore slot names) in one shared fn.
- **Tasks.** (1) detect spec files during indexing; (2) parse per format; (3) mint `Contract` nodes
  with normalized UIDs.
- **Tests.** An `openapi.yaml` with `POST /v1/approvals` → one `contract:http:POST:/v1/approvals`;
  a `.proto` service → `contract:grpc:…/Create`; `{id}` and `:id` normalize equal.
- **Acceptance.** Contract nodes queryable; normalization stable.
- **Effort.** M. **Deps.** none.

### F2.2 — Same-repo `IMPLEMENTS_CONTRACT` (Spring + NestJS)
- **Goal.** Link handler Symbols to their Contract (highest-precision lane).
- **Verified surface.** `EdgeType` has 14 variants (`edges.rs:5-20`); add `ImplementsContract` —
  additive (new `rel_table_name`, `Display`, and `insert_edge_with_conn` arm at `write.rs:385`).
  Handler signals via `framework_hint` (F2.0) + annotation/decorator extraction in the tree-sitter
  path.
- **Schema/signatures.** `EdgeType::ImplementsContract`; edge `{confidence}`.
- **Tasks.** (1) detect Spring `@PostMapping`/`@RequestMapping` + class-level base path, NestJS
  `@Post()` + controller prefix; (2) normalize verb+path; (3) match to a Contract node (or mint a
  code-derived one if no spec); (4) emit edge: 1.0 exact, 0.8 if base-path inferred.
- **Tests.** A Spring handler for `POST /v1/approvals` links to the spec's contract at 1.0;
  base-path-only inference lands at 0.8.
- **Acceptance.** Handlers in a Spring/NestJS fixture link to contracts.
- **Effort.** M. **Deps.** F2.0, F2.1.

### F2.3 — Safe cross-repo `CONSUMES_CONTRACT` (gRPC + operationId clients)
- **Goal.** Cross-repo edges only in the unambiguous lanes: gRPC `Service/Method` calls and typed
  clients matched by `operationId`. **Excludes** raw HTTP-literal `fetch`/axios matching (F2-v2).
- **Schema/signatures.** `EdgeType::ConsumesContract`.
- **Tasks.** (1) **demand gate first** — count actual cross-repo gRPC/typed-client couplings in the
  target repos (ADDENDUM §C); if negligible, stop here and revisit; (2) detect gRPC stub calls →
  `package.Service/Method`; (3) detect generated/typed-client calls referencing an `operationId`;
  (4) emit `CONSUMES_CONTRACT` to the matching Contract at 1.0 (exact FQN/operationId).
- **Tests.** A gRPC client call links cross-repo to the server's `IMPLEMENTS` contract; an
  operationId client call links to its spec contract.
- **Acceptance.** RFC F2 acceptance (`cross-repo-contracts --link-type contract` returns ≥2 for a
  real coupling); generic paths produce **no** edges (collision guard).
- **Effort.** M–L. **Deps.** F2.1, F2.2. **Gate.** demand count > threshold.

### F2.4 — Drift diagnostics
- **Goal.** "Declared-but-not-implemented" and "implemented-but-undeclared" reports (high value,
  near-free once F2.1/F2.2 exist).
- **Schema/signatures.** CLI `nestweaver contracts drift [--repo …]` →
  `{declared_only:[…], implemented_only:[…]}`.
- **Tasks.** Set-difference Contract nodes vs `IMPLEMENTS_CONTRACT` edges per repo.
- **Tests.** A spec endpoint with no handler appears in `declared_only`.
- **Acceptance.** Drift list matches a hand-checked fixture.
- **Effort.** S. **Deps.** F2.1, F2.2.

> **Wire into `cross_repo_contracts`:** extend the existing tool's response with a
> `link_type: "contract"` discriminator so contract-mediated links appear alongside name matches.
> Re-budget as new code (the current tool doesn't do contract matching).

---

## Wave 2 build order

```
F9.1 broken-links ┐
F9.2 orphans      ├─► F9.5 doc-stats        (F9.3 topic-clusters, F9.4 tag-graph: parallel)
F9.4 tag-graph    ┘

F2.0 wire detect_frameworks ─► F2.2 IMPLEMENTS ─┐
F2.1 contract nodes ────────────┴───────────────┼─► F2.4 drift
                                                 └─► F2.3 cross-repo (after demand gate)
```

**Deferred to F2-v2 (still in scope, sequenced after core proves out):** general HTTP-literal
cross-repo `CONSUMES` (Express/Go/`fetch`/axios) and GraphQL consumers — the lanes the evidence
(≤0.79 F1, generic-path collisions) says can't yet be made trustworthy. Revisit once the core ships
and the demand count justifies the false-positive risk.
