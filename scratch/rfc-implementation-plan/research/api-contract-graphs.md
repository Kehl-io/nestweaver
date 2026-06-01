# RFC Research Foundation — Framework-Aware Auto-Linking via API Contracts

**Feature:** Detect and parse OpenAPI/Swagger, `*.proto` (gRPC), and `*.graphql` schemas during indexing; create `Contract` nodes with stable UIDs; emit `IMPLEMENTS_CONTRACT` edges from server handlers (same repo) and `CONSUMES_CONTRACT` edges from client callers (other repos). Goal: graph mirrors real production wire topology so cross-repo impact analysis works (e.g. "what breaks if I change `PUT /v1/approvals/{id}`").

**v1 target languages:** TypeScript (NestJS/Express), Java (Spring), Go (chi/gorilla mux).

**Author:** research agent · **Date retrieved:** 2026-05-29 · all URLs accessed 2026-05-29.

> Scope note: This document is a *research foundation*, not a design spec. Claims are grounded in retrieved sources; anything not directly verified is marked `[UNVERIFIED]`. Crate versions/licenses verified against the live crates.io JSON API and docs.rs on 2026-05-29.

---

## 1. Research Foundation

### 1.1 Consumer-driven contracts (the conceptual model NestWeaver should mirror)

The conceptual spine of this feature is the **provider contract / consumer contract** distinction from Ian Robinson's "Consumer-Driven Contracts: A Service Evolution Pattern" (Ian Robinson, on martinfowler.com, **published 2006-06-12**; retrieved <https://martinfowler.com/articles/consumerDrivenContracts.html>). The article defines three contract types that map cleanly onto our edge model:

- **Provider contract** — "a service provider's business function capabilities … the set of exportable elements necessary to support that functionality." Singular, authoritative, closed. → In NestWeaver this is the **`Contract` node**, derived from the OpenAPI/proto/GraphQL spec OR from the handler that defines the route.
- **Consumer contract** — captures one consumer's expectations of a provider. Open, incomplete, *multiple* (one per consumer), non-authoritative. → This is the **`CONSUMES_CONTRACT` edge** from a client caller.
- **Consumer-driven contract** — the union of all consumer expectations, derived back into a provider-shaped contract.

The key insight we inherit: a provider may expose a large surface, but the *actually-coupled* surface is the subset consumers reference. NestWeaver's graph can compute this directly (which `Contract` nodes have ≥1 `CONSUMES_CONTRACT` edge) — something neither static codegen nor a raw OpenAPI file gives you.

**Pact** (consumer-driven contract *testing*; docs <https://docs.pact.io/> and <https://docs.pact.io/getting_started/how_pact_works>) operationalizes this at *test time*: the consumer test runs against a Pact mock, Pact records the interactions into a JSON contract, publishes it to a **Pact Broker**, and the provider replays those requests in CI to verify it still satisfies "at least the data described in the minimal expected response." Relevance to us: Pact's broker is essentially a *runtime/test-time* version of the cross-repo edge graph we want to build *statically*. Pact contracts are an authoritative, machine-readable source of consumer→provider edges where they exist — a high-confidence signal NestWeaver could ingest later, but they only exist if the team writes Pact tests.

**Spring Cloud Contract** (Spring project page <https://spring.io/projects/spring-cloud-contract/>; Baeldung intro <https://www.baeldung.com/spring-cloud-contract>) is the JVM CDC tool: producer-side contracts in a Groovy/YAML DSL under `src/test/resources/contracts`, from which it generates provider verification tests and WireMock stubs that consumers reuse via Stub Runner. Relevance: another structured, parseable contract artifact, and confirms that in Java shops the "contract" frequently lives *outside* the OpenAPI file (in test DSLs). Our detector should not assume an OpenAPI doc is the only contract source.

### 1.2 The specifications (authoritative UID + matching semantics)

**OpenAPI Specification v3.1.0** (OpenAPI Initiative; retrieved <https://spec.openapis.org/oas/v3.1.0.html>):
- **Path templating** is defined verbatim as "the usage of template expressions, delimited by curly braces `{}`, to mark a section of a URL path as replaceable using path parameters." → This is why the contract UID path segment is `{id}`-style, and why our confidence model must treat `{id}` (OpenAPI/Spring) vs `:id` (Express) as the *same* slot.
- Path parameter values **cannot contain unescaped `/`, `?`, or `#`** (per RFC3986). → A `{path}` template slot matches exactly one path segment unless it's an explicit catch-all; useful for normalizing.
- **`operationId`** is **case-sensitive** and **MUST be unique among all operations** in the document. → Best stable secondary key for HTTP contracts when present; but it is optional, so the primary UID must be derivable from `method + templated path`.
- **No `basePath`** in 3.1 (that was OAS 2.0). 3.1 uses a **Server Object** with a `url`; empty/absent servers default to `/`; server URLs may be relative and support variable substitution; the path is appended to the expanded server URL. → Base-path handling is a real normalization burden (see §4).

**gRPC / Protocol Buffers** (proto3 service naming; Buf "Files and packages" <https://buf.build/docs/reference/protobuf-files-and-packages/>; protobuf.dev style guide <https://protobuf.dev/programming-guides/style/>; Microcks gRPC conventions <https://microcks.io/documentation/references/artifacts/grpc-conventions/>):
- The **package** defines the namespace; all elements (messages, enums, services) are prefixed with the fully-qualified package name.
- The canonical wire/method identifier is **`package.ServiceName/MethodName`** (e.g. `io.github.microcks.grpc.hello.v1.HelloService/greeting`). This is also the literal HTTP/2 `:path` gRPC uses on the wire. → This is the *exact* string our `contract:grpc:...` UID should adopt; matching is unambiguous (no templating problem), which makes gRPC the *highest-confidence* contract type.
- Convention: service definitions use a `Service` suffix; packages are lowercase and typically versioned (`.v1`).

**GraphQL** — schema definition language (SDL): `type Query`, `type Mutation`, `type Subscription` root types; field names are the operation identifiers. No first-party "endpoint" concept — every GraphQL operation hits one HTTP endpoint (typically `POST /graphql`), so the meaningful contract unit is the **root field** (`Query.<field>`, `Mutation.<field>`), not a URL. (GraphQL spec is the authoritative source; the matching unit is the resolver field name, which is far harder to detect on the client side because clients send opaque query strings.) `[Partially UNVERIFIED — GraphQL spec text not fetched in this pass; treat GraphQL as a lower-priority v2 target.]`

### 1.3 Academic / industrial work on static endpoint & cross-service extraction

This is a well-studied problem; NestWeaver is not inventing the technique, only integrating it into a graph store.

- **Schneider, Bakhtin, Li, Soldani, Brogi, Cerny, Scandariato, Taibi — "Comparison of Static Analysis Architecture Recovery Tools for Microservice Applications" (2024).** Preprint <https://arxiv.org/html/2412.08352v1>; journal version *Empirical Software Engineering*, Springer, <https://link.springer.com/article/10.1007/s10664-025-10686-2>. Compared 13 tools (9 executable). **Key empirical numbers for endpoint detection (Table 5c): RAD F1 = 0.79 (best), RAD-source 0.67, Code2DFD 0.66; several tools (MicroGraal, Prophet, ContextMap) scored 0.0.** Critical takeaway for our confidence model: even purpose-built academic tools top out around **F1 0.79 on REST endpoint extraction** — meaning our `IMPLEMENTS_CONTRACT` detection *will* miss and mis-fire, so the design must surface confidence and degrade gracefully rather than claim completeness. The paper also notes "basic architecture extraction can be achieved with high precision via simple parsing of deployment files," while deeper source analysis improves recall.

- **Schneider & Scandariato — "Automatic extraction of security-rich dataflow diagrams for microservice applications written in Java" (Code2DFD)**, *Journal of Systems and Software*, 2023. ACM <https://dl.acm.org/doi/10.1016/j.jss.2023.111722>; tool repos <https://github.com/tuhh-softsec/code2DFD> and <https://github.com/M3SOulu/EMSE2025SAR-code2DFD>. Approach is **keyword detection in source with traceability back to the source location** (precision 0.94 / recall 0.88 on its own DFD dataset). Validates a pragmatic strategy: detect framework signatures (annotations, import names, call patterns) rather than attempting full semantic dataflow — directly applicable to our tree-sitter + query approach.

- **Tomas Cerny et al. — Endpoint Dependency Matrix (EDM) / Data Dependency Matrix (DDM)** line of work on microservice dependency recovery via static analysis (e.g. "The Microservice Dependency Matrix", arXiv <https://arxiv.org/pdf/2309.02804>). Establishes the **endpoint-as-node, call-as-edge** matrix model that our `Contract` node + `CONSUMES_CONTRACT` edge directly implements. `[Author attribution per search synthesis; exact author list/year per-paper not individually fetched — UNVERIFIED in detail.]`

- **"Microvision: Static analysis-based approach to visualizing microservices"** (arXiv <https://arxiv.org/pdf/2207.02974>) — AST-walk extraction of call graphs from microservice source. Confirms AST/tree-sitter walking + method-call recognition as the standard endpoint/caller extraction technique. `[Author/year not individually fetched — UNVERIFIED.]`

- **"Collecting Service-Based Maintainability Metrics from RESTful API Descriptions: Static Analysis and Threshold Derivation"** (arXiv <https://arxiv.org/pdf/2007.10405>) — static analysis driven from OpenAPI descriptions; relevant prior art for spec-as-ground-truth. `[Author/year not individually fetched — UNVERIFIED.]`

---

## 2. Prior Art / Projects + Tradeoffs (static vs runtime topology)

| Project / approach | What it does | Relevance / tradeoff |
|---|---|---|
| **Backstage Software Catalog** (<https://backstage.io/docs/features/software-catalog/descriptor-format/>, API entity docs <https://backstage.io/docs/features/software-catalog/software-catalog-api/>) | First-class **`kind: API`** entity with `spec.type` ∈ {openapi, asyncapi, graphql, grpc} and a `definition` (inline or `$text:` ref). Components declare `providesApis` / `consumesApis`. | **This is the closest industrial model to our feature.** Validates exactly our node taxonomy (one API kind, typed by openapi/grpc/graphql) and our two edge directions (provides/consumes). *Key difference:* Backstage relies on **humans hand-authoring** `providesApis`/`consumesApis` in `catalog-info.yaml`; NestWeaver's value-add is *deriving these edges automatically from code*. We should adopt Backstage's vocabulary so output is interoperable. |
| **Sourcegraph** cross-repo code intelligence | Cross-repo definition/reference search via SCIP/LSIF indexes. | Cross-repo navigation precedent, but operates at the **symbol** level (same model NestWeaver uses today), *not* the wire/contract level. Does not understand that `fetch('/v1/approvals')` in repo A targets a handler in repo B. Our contract edges are the missing layer. |
| **OpenTelemetry service maps / Service Graph Connector** (<https://oneuptime.com/blog/post/2026-02-06-service-graph-connector-opentelemetry-collector/view>) | **Runtime** topology: matches client/server span pairs from distributed traces to emit per-edge metrics (rate/error/latency). | **The principal contrast.** Runtime maps "always reflect reality" and capture *dynamic* routes static analysis misses — but require running production with instrumentation, only show paths that *executed* during the window, and can't answer "what *would* break if I change this" for cold/seasonal paths. **Static (our approach) sees all code-declared paths regardless of traffic, works pre-deploy, needs no instrumentation, but suffers false negatives/positives (F1 ≤ ~0.79 per §1.3).** The honest framing for the RFC: NestWeaver provides the *design-time* wire graph; it is complementary to, not a replacement for, OTel runtime maps. A future enhancement could *reconcile* the two (confirm/upgrade static edges with runtime evidence). |
| **buf / grpcurl / prost / tonic** (Protobuf tooling) | Parse and reflect over `.proto`; buf does linting/breaking-change detection. | Confirms `.proto` is trivially and reliably machine-parseable to the `package.Service/Method` level. gRPC is the **lowest-risk** contract type to implement. buf's breaking-change detection is conceptually what our `impact` query delivers for proto contracts. |
| **OpenAPI-driven client/server codegen** (openapi-generator, etc.) | Generate typed clients/servers from a spec. | Two implications: (1) where a generated client exists, the consumer-side calls go through a typed stub whose method names map back to `operationId` — a **high-confidence** caller-detection path; (2) generated code is voluminous and may pollute the symbol graph — detection must recognize generated-client patterns specifically. |
| **Pact Broker / Spring Cloud Contract** | Test-time consumer-driven contract artifacts (§1.1). | Authoritative consumer→provider edges *where present*. Candidate **high-confidence ingestion source** for a later milestone; not required for v1. |
| **Microcks** | Mock/test platform that ingests OpenAPI, gRPC, GraphQL, AsyncAPI; integrates with Backstage. | Confirms the multi-format contract catalog is a real product category and that our three formats are the standard set. |

**Net positioning for the RFC:** the unfilled niche is a *static, multi-repo, code-derived* contract graph that (a) needs no runtime instrumentation, (b) needs no human-authored catalog YAML, and (c) lives in the same graph store as symbol-level edges so impact queries can traverse symbol → handler → contract → consumer-symbol-in-another-repo. No single tool above does all three.

---

## 3. Recommended Approach for NestWeaver

### 3.1 Contract parsing (producing `Contract` nodes)

Two independent producers of `Contract` nodes, merged by UID:

1. **Spec files** (authoritative). Discover during the file walk by extension + content sniff:
   - OpenAPI/Swagger: `*.yaml`/`*.yml`/`*.json` whose root has `openapi:` or `swagger:`. Parse with a typed crate (see §5). Emit one `Contract` per `(path, method)` operation.
   - gRPC: `*.proto`. Parse to a `FileDescriptorSet`; emit one `Contract` per `service` × `rpc`.
   - GraphQL: `*.graphql`/`*.gql` SDL. Emit one `Contract` per root-type field (`Query.x`, `Mutation.x`). (Lower priority — see §3.5.)
2. **Code-derived** (when no spec exists). Server handler detection (§3.2) *also* mints a `Contract` node if the spec didn't already declare it — this is how repos without an OpenAPI file still get nodes. Spec-derived nodes are confidence 1.0 as contracts; purely code-derived nodes carry the handler's detection confidence.

Merge rule: a spec-derived node and a code-derived node with the same UID are the same node; the `IMPLEMENTS_CONTRACT` edge then links the handler symbol to it.

### 3.2 Per-framework handler detection → `IMPLEMENTS_CONTRACT` (same repo)

Detection is **tree-sitter query + small regex/heuristics** over the framework signatures, mirroring the Code2DFD keyword-evidence strategy. Per v1 target:

- **Java / Spring** (`tree-sitter-java`, already a dep): annotations `@RequestMapping`, `@GetMapping`/`@PostMapping`/`@PutMapping`/`@DeleteMapping`/`@PatchMapping` on methods, with class-level `@RequestMapping` contributing the base path. Verb is implied by the annotation; path is the annotation's `value`/`path`. This is the **best-supported, highest-precision** case (RAD's 0.79 F1 came from exactly these annotations). Confidence 1.0 when both class+method paths are string literals.
- **TypeScript / NestJS** (`tree-sitter-typescript`, already a dep): controller class decorator `@Controller('base')` + method decorators `@Get()/@Post()/@Put()/@Delete()/@Patch('sub')`. Structurally identical to Spring; high precision.
- **TypeScript / Express**: method calls `app.get('/path', handler)` / `router.post(...)` etc. Verb = method name; path = first string-literal argument; the handler symbol = the function passed. Router mounting (`app.use('/v1', router)`) contributes a base path that must be threaded — a known source of recall loss.
- **Go / chi & gorilla mux**: `r.Get("/path", handler)` / `r.HandleFunc("/path", handler)` / `r.Methods("POST")` chains; chi sub-routers via `r.Route("/v1", func(r){...})` / `r.Mount`. Verb extraction for `HandleFunc` may require following a `.Methods(...)` call → lower confidence when verb is not literal.

For each detected handler emit `IMPLEMENTS_CONTRACT(handler_symbol → contract_node)`.

### 3.3 Per-framework caller detection → `CONSUMES_CONTRACT` (cross-repo)

Harder and lower-confidence (this is where F1 drops). v1 focus = TypeScript clients, since that's the stated consumer language:

- **`fetch` / axios / got literals**: `fetch('/v1/approvals', { method: 'POST' })`, `axios.post('/v1/approvals', ...)`, `api.get(\`/v1/approvals/${id}\`)`. Extract URL string + verb. Template-literal interpolation (`${id}`) → normalize the interpolated segment to a templated slot for matching (confidence 0.8, parameterized).
- **Generated/typed clients**: a call to a generated client method whose name derives from `operationId` (e.g. `client.createApproval()`); match via `operationId` when the spec is also indexed (high confidence) — otherwise undetectable from the opaque method name alone.
- **Base-URL composition**: callers usually compose `\`${BASE_URL}/v1/approvals\``; the literal segment is recoverable, the host is not — which is fine because matching is on path+verb, not host (see §4).

Emit `CONSUMES_CONTRACT(caller_symbol → contract_node)`. Cross-repo linking reuses NestWeaver's existing multi-repo store: contract UID is repo-independent, so a consumer edge in repo A and an implements edge in repo B converge on the same node automatically.

### 3.4 UID scheme

Stable, human-readable, repo-independent (matches the feature brief and the gRPC wire convention):

- HTTP: `contract:http:<VERB>:<normalized-templated-path>` → `contract:http:POST:/v1/approvals`, `contract:http:PUT:/v1/approvals/{id}`
- gRPC: `contract:grpc:<package>.<Service>/<Method>` → `contract:grpc:approvals.v1.Approvals/Create` (the literal proto3 FQN per §1.2 — no normalization needed)
- GraphQL: `contract:graphql:<RootType>.<field>` → `contract:graphql:Mutation.createApproval`

**Path normalization (HTTP) is the load-bearing step** and must be applied identically on both producer (handler) and consumer (caller) sides:
1. Lowercase the verb-prefix consistently (store verb uppercase).
2. Collapse all parameter syntaxes to one canonical form: `:id` (Express/chi), `{id}` (OpenAPI/Spring), `${id}` (TS template), `<id>` → all become `{id}` for matching, and slot *names* are ignored when comparing (so `{id}` ≡ `{approvalId}`).
3. Strip trailing slashes; normalize duplicate slashes; resolve mounted base paths before UID construction.
4. Keep the *original* path string as a node property for display/debugging.

### 3.5 What's realistically automatable per language (honest assessment)

| Target | Handler/`IMPLEMENTS` | Caller/`CONSUMES` | Notes |
|---|---|---|---|
| **Spring (Java)** | **High** (annotations, literal paths) — best case in the literature | n/a for v1 (Java-as-consumer is v2) | Class+method path join + base-path; RestTemplate/WebClient/Feign callers are a known-feasible v2 add. |
| **NestJS (TS)** | **High** (decorators) | **Medium–High** for generated/typed clients via `operationId` | Decorator path join mirrors Spring. |
| **Express (TS)** | **Medium** (literal first-arg; router mounting hurts recall) | **Medium** (`fetch`/axios literals; template-literal slots → 0.8) | Dynamic route construction is the main miss. |
| **chi / gorilla mux (Go)** | **Medium** (`r.Get`/`Route`/`Mount` literal; `HandleFunc`+`.Methods` verb chaining lowers confidence) | n/a for v1 | Sub-router base-path threading is the recall risk. |
| **gRPC (all)** | **High** (`.proto` is fully parseable; no templating) | **High** where a generated stub call resolves to `Service/Method` | Lowest-risk contract type overall. |
| **GraphQL** | Medium (SDL parse is easy; mapping resolvers→fields is framework-specific) | **Low** (clients send opaque query strings; matching root fields out of a query document is hard) | Recommend **v2/deferred**; parse SDL to nodes but don't promise robust consumer edges. |

Recommended **v1 cut**: spec parsing for all three formats (cheap, high value as nodes), `IMPLEMENTS_CONTRACT` for Spring + NestJS + Express + chi/gorilla + gRPC, `CONSUMES_CONTRACT` for TS `fetch`/axios + typed-client-via-operationId. Defer GraphQL consumer edges and Java/Go consumers to v2.

### 3.6 Confidence model

Per the brief, anchored by the §1.3 finding that even dedicated tools cap near F1 0.79 — so **confidence must be visible and edges must never be presented as ground truth**:

- **1.0** — exact path + verb match between contract and handler/caller, both literal (or gRPC `Service/Method`, or `operationId` match against an indexed spec).
- **0.8** — parameterized-path match where slot syntax differs (`{id}` vs `:id` vs `${id}`) but structure aligns; or verb inferred from annotation rather than literal.
- **0.5** — inferred: dynamically-constructed path partially recovered, base path guessed, or content-type/version disambiguation uncertain.

Store confidence on the edge (NestWeaver edges already carry confidence; dead-code/PPR already threshold on it). Impact queries should let users filter by minimum contract-edge confidence so a 0.5 inferred edge doesn't generate false "this will break" alarms.

---

## 4. Pitfalls / Failure Modes + Mitigations

1. **Path-templating syntax mismatch** (`{id}` vs `:id` vs `${id}` vs `<id>`). *Mitigation:* single canonical normalization (§3.4) applied identically on both sides; ignore slot *names* in matching. (Grounded in OAS 3.1 curly-brace templating + Express colon convention.)
2. **Base paths / route mounting** (`app.use('/v1', router)`, class-level `@RequestMapping("/v1")`, chi `r.Route("/v1", ...)`, OAS Server `url`). The single biggest recall killer per the literature. *Mitigation:* resolve mount/prefix context during the AST walk before minting the UID; where base path can't be statically resolved, mint with the partial path at confidence 0.5 and flag.
3. **Versioning & content negotiation** (`/v1/` vs `/v2/`, `Accept` headers, media-type routing). *Mitigation:* version is part of the path string so it naturally distinguishes nodes; do **not** collapse versions. Content-negotiation variants share a UID (path+verb) — acceptable for v1; note as a known limitation.
4. **Host/base-URL is unknowable statically** for callers. *Mitigation:* deliberately match on **path+verb only**, not host (consistent with the UID scheme). Accept the resulting risk of false cross-repo matches when two unrelated services both expose `POST /v1/login` (see #6).
5. **Dynamic / computed routes** (paths built from variables, route tables in config, regex routes). *Mitigation:* recover the literal prefix where possible at confidence 0.5; otherwise skip. Document as the primary false-negative source. This is exactly why NestWeaver should frame itself as *complementary to* OTel runtime maps (§2), which catch these.
6. **False matches across services** (same `POST /v1/login` in two unrelated apps; generic `/health`, `/metrics`). *Mitigation:* (a) maintain a denylist of ubiquitous paths (`/health`, `/metrics`, `/`, `/favicon.ico`); (b) where instance config groups repos into a service/feature, prefer intra-feature matches and down-weight or suppress cross-feature collisions; (c) surface ambiguity rather than silently picking one (NestWeaver already has an "ambiguous" exit code 3 convention).
7. **Generated-client noise** (codegen produces thousands of symbols). *Mitigation:* recognize generated-client signatures and either tag those symbols or route their calls straight to `operationId`-based high-confidence edges instead of treating them as ordinary code.
8. **Spec drift** (OpenAPI file out of date vs handlers). *Mitigation:* because we mint nodes from *both* spec and code, a spec contract with no `IMPLEMENTS_CONTRACT` edge (or vice versa) is itself a useful signal — expose it (a "declared but not implemented" / "implemented but undocumented" diagnostic).
9. **operationId optionality** — can't rely on it as the primary key. *Mitigation:* primary UID = method+templated path; `operationId` stored as a secondary index used only to match generated/typed clients.
10. **gRPC streaming / GraphQL subscriptions** — long-lived, non-request/response. *Mitigation:* still model as contract nodes; don't over-promise impact semantics in v1.

---

## 5. Rust Crate Assessment (verified on crates.io / docs.rs, 2026-05-29)

All versions/licenses pulled live from the crates.io JSON API on 2026-05-29.

### OpenAPI

| Crate | Latest | License | Status | Recommendation |
|---|---|---|---|---|
| **`openapiv3`** | **2.2.0** (updated 2025-06-02; ~9.3M downloads) | MIT/Apache-2.0 | Mature, widely used. Serde data structures for OpenAPI **3.0.x**. | **Recommend for OAS 3.0.** Stable, popular, dual-licensed (compatible). Does *not* cover 3.1. |
| **`oas3`** | **0.22.0** (updated 2026-05-06; ~1.16M downloads) | MIT | Actively maintained; parses/validates OpenAPI **3.1.x**. | **Recommend for OAS 3.1.** Newer-but-pre-1.0 API (expect churn). MIT-only (still compatible). Use alongside `openapiv3` to cover 3.0 + 3.1, or standardize on `oas3` if 3.1-first is acceptable. |
| `swagger` | 7.0.0 | Apache-2.0 | Utilities for OpenAPI-Generator output, **not a spec parser**. | **Do not use** for parsing — wrong tool. |

YAML/JSON loading: project already depends on **`serde_yaml` 0.9** (note: `serde_yaml` is **deprecated/unmaintained** — last release 0.9.34+deprecated, 2024). For new contract parsing prefer **`serde_yaml_ng` 0.10.0** (MIT) or the YAML-1.2 **`saphyr` 0.0.6** (pre-1.0). `serde_json` already in use for JSON specs. **Action item:** OpenAPI YAML parsing is a good reason to migrate off deprecated `serde_yaml`.

### gRPC / Protobuf

| Crate | Latest | License | Status | Recommendation |
|---|---|---|---|---|
| **`protobuf-parse`** | **3.7.2** (updated 2025-03-10; ~29.5M downloads) | MIT | Mature. **Has a pure-Rust parser requiring no `protoc` binary** (verified on docs.rs: "pure rust parser (no dependencies)"); `protoc` mode optional. **Caveat (verified): docs state it "is not meant to be used directly" and has "no stable API."** | **Recommend with caution** — best pure-Rust path-into-`FileDescriptorSet` but unstable API; pin exact version. Avoids a `protoc` system dependency, which matters for NestWeaver's zero-config indexing. |
| **`protox`** | **0.9.1** (updated 2025-12-02; ~10.8M downloads) | MIT OR Apache-2.0 | A pure-Rust protobuf **compiler** producing `FileDescriptorSet`; no `protoc` needed. | **Recommend (preferred).** Pure-Rust, dual-licensed, designed to be used directly (unlike `protobuf-parse`). Best fit for parsing `.proto` to services/methods without external tooling. |
| `prost` / `prost-build` / `tonic-build` | 0.14.3 / 0.14.3 / 0.14.6 | Apache-2.0 / Apache-2.0 / MIT | Mature, huge usage (`prost` ~428M downloads). Codegen-oriented; `prost-build`/`tonic-build` traditionally lean on `protoc` (protox can back them). | Not needed for *parsing-only*; use `protox` for descriptors. Listed for completeness. |
| `prost-reflect` | 0.16.4 (updated 2026-05-24; ~59M downloads) | MIT OR Apache-2.0 | Mature; runtime reflection over compiled descriptors. | Useful if we want dynamic message reflection later; **not required** for service/method extraction. |
| `protobuf` | max stable 3.7.2 (4.x in pre-release) | BSD-3-Clause | Mature (Google data-interchange runtime). | Note BSD-3-Clause license (still permissive/compatible). Transitive via `protobuf-parse`. |
| `protobuf-parser` (singular) | 0.1.3, **last updated 2018** | MIT | **Abandoned.** | **Do not use.** |

**gRPC recommendation: `protox` (primary) → `FileDescriptorSet` → walk services/methods.** Pure-Rust, directly usable, dual-licensed, no `protoc` dependency. `protobuf-parse` is a fallback.

### GraphQL

| Crate | Latest | License | Status | Recommendation |
|---|---|---|---|---|
| **`apollo-parser`** | **0.8.6** (updated 2026-05-14; ~886K downloads) | MIT OR Apache-2.0 | Actively maintained by Apollo; **spec-compliant, error-resilient** GraphQL parser. | **Recommend** for GraphQL SDL parsing (v2). Best-maintained, dual-licensed. |
| `async-graphql-parser` | 7.2.1 stable (8.0.0-rc; ~27.6M downloads) | MIT OR Apache-2.0 | Mature; the parser behind `async-graphql`. | Viable alternative; tied to the async-graphql ecosystem. |
| `graphql-parser` | 0.4.1 (updated 2024-12-03; ~30M downloads) | MIT/Apache-2.0 | Widely used but **lightly maintained** (last release 2024). | Acceptable but prefer `apollo-parser` for active maintenance. |

### Effort estimate per target (relative)

- **gRPC contract nodes:** **Low** — `protox` → descriptor walk is mechanical; UID is the literal FQN. Highest value-to-effort.
- **OpenAPI contract nodes:** **Low–Medium** — `oas3`/`openapiv3` give typed operations; effort is in server-object/base-path normalization and 3.0-vs-3.1 dual support.
- **Spring `IMPLEMENTS`:** **Medium** — tree-sitter-java queries for annotations + class/method path join; literature shows this is the most tractable handler case.
- **NestJS `IMPLEMENTS`:** **Medium** — analogous decorator queries in tree-sitter-typescript.
- **Express `IMPLEMENTS`:** **Medium–High** — literal-arg extraction is easy; router-mount base-path threading is the real work.
- **chi/gorilla `IMPLEMENTS`:** **Medium–High** — verb extraction (`HandleFunc` + `.Methods`) and sub-router base paths add complexity.
- **TS `CONSUMES` (fetch/axios + operationId):** **High** — string/template-literal recovery, generated-client recognition, and the cross-repo false-match guards (§4 #6) are the most error-prone surface and where confidence buckets earn their keep.
- **GraphQL consumer edges:** **High / defer to v2** — opaque client query strings make robust matching impractical for v1.

All grammars needed are **already workspace dependencies** (`tree-sitter-java` 0.23, `tree-sitter-typescript` 0.23, `tree-sitter-go` 0.25), so no new parser-grammar onboarding is required for handler/caller detection — only the spec crates (`oas3`/`openapiv3`, `protox`, optionally `apollo-parser`) are net-new.

---

## Sources (all retrieved 2026-05-29)

- Ian Robinson, "Consumer-Driven Contracts: A Service Evolution Pattern," martinfowler.com, 2006-06-12 — <https://martinfowler.com/articles/consumerDrivenContracts.html>
- Pact docs — <https://docs.pact.io/> ; <https://docs.pact.io/getting_started/how_pact_works>
- Spring Cloud Contract — <https://spring.io/projects/spring-cloud-contract/> ; Baeldung intro <https://www.baeldung.com/spring-cloud-contract>
- OpenAPI Specification v3.1.0 — <https://spec.openapis.org/oas/v3.1.0.html>
- Buf "Files and packages" — <https://buf.build/docs/reference/protobuf-files-and-packages/> ; Protobuf style guide <https://protobuf.dev/programming-guides/style/> ; Microcks gRPC conventions <https://microcks.io/documentation/references/artifacts/grpc-conventions/>
- Schneider et al., "Comparison of Static Analysis Architecture Recovery Tools for Microservice Applications," 2024 — arXiv <https://arxiv.org/html/2412.08352v1> ; Empirical Software Engineering (Springer) <https://link.springer.com/article/10.1007/s10664-025-10686-2>
- Schneider & Scandariato, "Automatic extraction of security-rich dataflow diagrams … (Code2DFD)," JSS 2023 — ACM <https://dl.acm.org/doi/10.1016/j.jss.2023.111722> ; repos <https://github.com/tuhh-softsec/code2DFD> , <https://github.com/M3SOulu/EMSE2025SAR-code2DFD>
- "The Microservice Dependency Matrix" (Cerny et al.) — arXiv <https://arxiv.org/pdf/2309.02804>
- "Microvision" — arXiv <https://arxiv.org/pdf/2207.02974>
- "Collecting Service-Based Maintainability Metrics from RESTful API Descriptions" — arXiv <https://arxiv.org/pdf/2007.10405>
- Backstage Software Catalog descriptor format — <https://backstage.io/docs/features/software-catalog/descriptor-format/> ; software-catalog API <https://backstage.io/docs/features/software-catalog/software-catalog-api/>
- OpenTelemetry Service Graph Connector — <https://oneuptime.com/blog/post/2026-02-06-service-graph-connector-opentelemetry-collector/view>
- crates.io JSON API (version/license/maintenance) and docs.rs for: openapiv3, oas3, swagger, protobuf-parse, protox, prost, prost-build, tonic-build, prost-reflect, protobuf, protobuf-parser, apollo-parser, async-graphql-parser, graphql-parser, serde_yaml, serde_yaml_ng, saphyr — queried 2026-05-29.
