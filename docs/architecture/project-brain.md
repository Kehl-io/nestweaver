# NestWeaver Project Brain — Architecture & Design

**Status:** Design draft
**Date:** 2026-05-23
**Audience:** Engineers implementing the system; reviewers evaluating the approach
**Companion doc:** `docs/plans/markdown-brain-extension.md` (effort and phasing plan)

---

## 1. Executive Summary

### What this is

The **Project Brain** is an extension to NestWeaver that turns it from a code-only knowledge graph into a unified, queryable model of a working software engineer's entire context: code repositories, Obsidian-style markdown vaults (PRDs, design docs, meeting notes, decisions, journals), tags, projects, and the cross-domain links between them. It is exposed to Claude (or any MCP-capable agent) as a persistent, low-latency "brain" — the agent queries structured context instead of dumping raw files into a window.

The single-sentence pitch: **precomputed Personalized PageRank over a unified code+docs graph, served via MCP, with token-budgeted output and incremental updates measured in milliseconds.**

### Why it matters

LLM context is the binding constraint on agent quality. Every token spent on irrelevant material is a token not spent on reasoning, and every retrieval round-trip is latency the user feels. Two failure modes dominate today:

1. **Naive retrieval (read whole files, paste whole vaults).** Burns the budget on noise. Karpathy's wiki experiment showed structured markdown can reduce tokens by 20–40× compared to raw chat history, but the gains evaporate at vault scale without principled ranking.
2. **Embedding-only RAG.** Surface-level semantic similarity misses multi-hop structural relevance ("the function called by the function described in this PRD"), and embedding lookups are expensive at scale.

The Project Brain uses **graph structure as the primary signal** and embeddings only for coverage. The retrieval algorithm is **Personalized PageRank (PPR)** — the same primitive Aider's repo-map uses to outperform naive context inclusion, the same primitive HippoRAG uses to achieve 10–30× cheaper multi-hop reasoning than chain-of-thought RAG, and the same primitive TERAG uses to hit 80%+ accuracy at 3–11% of the token cost of competing methods.

### Why the research backs this design

- **TERAG** (Yin et al., 2025, arXiv:2509.18667) demonstrates that PPR-based graph retrieval over LLM-extracted entity/relation graphs achieves competitive accuracy while reducing output tokens by 89–97% versus baselines. Their result generalises: PPR is a near-optimal context-selector when the graph faithfully encodes structural relationships.
- **HippoRAG** (Gutiérrez et al., NeurIPS 2024) frames the architecture neurobiologically: the LLM is the neocortex (pattern completion, abstraction), the knowledge graph + PPR is the hippocampus (associative memory, cross-episode recall). This brain analogy is not metaphorical decoration — it predicts which workloads benefit most: multi-hop reasoning, cross-document synthesis, and path-finding queries that embeddings alone cannot answer.
- **RisGraph** (Feng et al., SIGMOD 2021) shows sub-millisecond per-update PPR maintenance on evolving graphs at millions of operations per second. This is what makes the brain *live*: a file watcher can re-rank the graph faster than the user can switch windows.
- **Aider's repo-map** is a production existence proof — Paul Gauthier's analysis shows PPR-scored context produces measurably higher coding accuracy than naive inclusion.

### What it delivers

| Capability | Today (Claude + Obsidian) | With Project Brain |
|---|---|---|
| "What's the status of project X?" | Agent searches, opens 5 files, summarises | One MCP call, ~300 tokens out |
| "What code implements this PRD?" | Manual cross-reference, often impossible | `code_for(prd)` returns ranked symbols |
| "Give me context to work on Y in 4K tokens" | Agent guesses, over- or under-fetches | `brain_context(seeds, 4000)` returns exactly that |
| Multi-hop ("decisions about auth that affect mobile") | Multiple searches, manual stitching | Single PPR seed, multi-hop ranked output |
| Latency for context retrieval | Seconds (file I/O + LLM thinking) | <100ms (precomputed PPR lookup) |
| Reaction to a vault edit | None (must re-search) | Sub-second incremental update |

---

## 2. System Architecture

### 2.1 High-level component diagram

```
                       ┌─────────────────────────────────────────────┐
                       │              MCP Client (Claude)            │
                       └────────────────────┬────────────────────────┘
                                            │ MCP (stdio / SSE)
                       ┌────────────────────▼────────────────────────┐
                       │   nestweaver-mcp  (rmcp server)             │
                       │   tools: brain_context, brain_search,       │
                       │          note_get, backlinks, projects, ... │
                       └────────────────────┬────────────────────────┘
                                            │ engine API
              ┌─────────────────────────────▼──────────────────────────────┐
              │                  nestweaver-engine                          │
              │  ┌────────────┐  ┌──────────────┐  ┌──────────────────┐    │
              │  │ Connectors │  │ Query layer  │  │ Incremental sync │    │
              │  │  Code      │  │ brain_context│  │ file watcher     │    │
              │  │  Markdown  │  │ search       │  │ delta computer   │    │
              │  │  (future)  │  │ token budget │  │ ppr updater      │    │
              │  └─────┬──────┘  └──────┬───────┘  └────────┬─────────┘    │
              └────────┼────────────────┼─────────────────────┼────────────┘
                       │                │                     │
        ┌──────────────▼──┐   ┌─────────▼────────┐   ┌────────▼──────────┐
        │ nestweaver-     │   │ nestweaver-store │   │ in-process caches │
        │   parser        │   │  LadybugDB +     │   │  PPR vectors      │
        │  tree-sitter    │   │  schema + DDL    │   │  adjacency lists  │
        │  comrak (md)    │   │  read/write/ppr  │   │  bm25 index       │
        └──────────────┬──┘   └────────┬─────────┘   │  vector index     │
                       │               │             └───────────────────┘
                       ▼               ▼
              ┌────────────────────────────────────────────┐
              │       on-disk: nestweaver.lbug             │
              │       sidecars: .pagerank.json,            │
              │                 .ppr-incremental.bin,      │
              │                 .embeddings.bin,           │
              │                 .manifests.json            │
              └────────────────────────────────────────────┘
```

### 2.2 Data flow

**Indexing (full):**
```
filesystem walk
   → connector dispatch by extension/format
       → CodeConnector (tree-sitter)        ─┐
       → MarkdownConnector (comrak + yaml)  ─┴→ ParsedBatch (typed nodes + edges)
                                                  → resolver pass (wikilinks, cross-domain)
                                                     → batch insert into LadybugDB
                                                        → compute PPR over unified graph
                                                           → persist PPR vector to sidecar
                                                              → build BM25 + vector indices
```

**Indexing (incremental):**
```
file watcher event (modify/create/delete)
   → diff file vs. content_hash → unchanged? skip
   → re-parse changed file → new ParsedFile
   → diff against previous nodes/edges for that file
       → compute (nodes_added, nodes_removed, edges_added, edges_removed)
           → apply delta to store
              → incremental PPR update (forward push, RisGraph-style)
                 → persist deltas
```

**Query:**
```
MCP tool call: brain_context(seeds, token_budget)
   → resolve seeds (names → UIDs, hybrid lookup)
       → load precomputed PPR vector from cache
          → for each seed: personalize → propagate (cached) → rank
             → token-budgeted truncation (greedy by relevance, respecting node-kind quotas)
                → enrich (signatures, snippets, file:line)
                   → JSON response
```

### 2.3 How it extends NestWeaver's existing architecture

| Crate | Status | Changes |
|---|---|---|
| `nestweaver-schema` | **Extended** | New node structs (`Vault`, `Note`, `Heading`, `Section`, `Tag`, `Project`); new edge variants; expand `NODE_LABELS`/`EDGE_LABELS` constants; new UID helpers (`vault_uid`, `note_uid`, `heading_uid`, `section_uid`, `tag_uid`, `project_uid`). |
| `nestweaver-parser` | **Extended** | New `markdown` module using `comrak` + `serde_yaml`. `SourceKind` enum supersedes the leaky use of `Language` at the parser boundary. Existing code parsing untouched. |
| `nestweaver-resolver` | **Extended** | New `lang/markdown.rs` with the 5-priority wikilink resolver. New top-level `cross_domain.rs` for `REFERENCES_CODE` resolution between docs and code. |
| `nestweaver-store` | **Major change** | Add DDL for all new node/rel tables. **Refactor `load_ppr_graph` to be `GraphScope`-aware** (highest-leverage single change in the crate). Add `incremental_ppr.rs` for live updates. Add insert/read methods per new node type. |
| `nestweaver-engine` | **Major change** | New `connectors/` submodule with `Connector` trait, `CodeConnector` (extracted from current `index.rs`), `MarkdownConnector` (new). New `sync/` submodule with `notify`-based file watcher. New `query/` extensions for unified retrieval and token budgeting. |
| `nestweaver-storage` | **Unchanged** | Snapshot transport is already domain-agnostic. |
| `nestweaver-mcp` | **Replace stub** | Implement against `rmcp` (the official Rust MCP SDK) with the tool set defined in §7. |
| Root CLI (`src/main.rs`) | **Extended** | New commands (`index-vault`, `note`, `notes`, `wikilinks`, `tag`, `project`, `brain`, `mcp serve`, `watch`). Existing commands gain `--include-notes` / `--scope` flags. |

The new crate `nestweaver-md-parser` is **not** recommended; keep Markdown in `nestweaver-parser` behind a module boundary. Avoiding workspace churn matters more than crate purity here.

---

## 3. Data Model

### 3.1 Node types

All nodes carry a globally unique `uid` and live in LadybugDB node tables. UIDs are SHA-256-truncated to 12 hex chars and prefixed with their type, matching the existing `repo:`/`file:`/`svc:`/`sym:` convention.

#### Existing (unchanged)

| Node | Prefix | Purpose |
|---|---|---|
| `Repo` | `repo:` | A code repository. |
| `File` | `file:` | A source file. |
| `Service` | `svc:` | A directory-grouped logical service. |
| `Symbol` | `sym:` | A function/class/method/interface. |

#### New — knowledge domain

| Node | Prefix | Purpose | Key properties |
|---|---|---|---|
| `Vault` | `vlt:` | An Obsidian vault root. Peer of `Repo`. | `uid`, `root_path`, `instance_id`, `indexed_at` |
| `Note` | `note:` | A markdown file inside a vault. Peer of `File`. | `uid`, `vault_uid`, `path`, `title`, `frontmatter` (JSON), `note_kind` (see §3.2), `content_hash`, `mtime`, `summary`, `embedding`, `pagerank_score` |
| `Heading` | `head:` | A heading in a note — addressable target of `[[Note#Heading]]`. | `uid`, `note_uid`, `level` (1–6), `text`, `slug`, `start_line`, `end_line`, `content_hash` |
| `Section` | `sec:` | The body text under a heading (or preamble). Unit of retrieval. | `uid`, `note_uid`, `heading_uid` (nullable), `start_line`, `end_line`, `text_hash`, `embedding`, `pagerank_score` |
| `Tag` | `tag:` | A `#tag` or `#nested/tag`. Canonical name = lowercased, slash-separated. | `uid`, `vault_uid`, `name`, `parent_tag_uid` (nullable) |
| `Project` | `proj:` | A logical grouping that spans vaults and repos. Generalises the existing `Feature` config. | `uid`, `name`, `summary`, `instance_id` |
| `ExternalLink` | `ext:` | A normalised external URL referenced from notes. Optional — only created if at least one section links to it. | `uid`, `url`, `host` |

#### Note subtypes — `note_kind` discriminator

Rather than introducing separate node types for `PRD`, `DesignDoc`, `MeetingNote`, etc. (which would require a node-type explosion and per-kind DDL), use a single `Note` node with a `note_kind` string property derived from:

1. Frontmatter `type:` field (highest priority).
2. Path heuristic (e.g. `PRDs/**/*.md` → `prd`, `meetings/**/*.md` → `meeting`).
3. Filename heuristic (`*-prd.md`, `*-decision.md`, `*-rfc.md`).
4. Default: `note`.

Known kinds: `note`, `prd`, `design`, `meeting`, `decision`, `rfc`, `journal`, `readme`, `changelog`. The set is open — clients filter by it but the system doesn't enforce membership. This avoids the schema churn that would follow from making each a node type, while still enabling typed queries like `notes --kind decision`.

### 3.2 Edge types

LadybugDB requires one REL table per `(FROM, TO)` pair. The taxonomy below is the logical model; the physical schema (§5) flattens it into per-pair tables.

#### Containment

| Edge | From → To | Direction | Meaning |
|---|---|---|---|
| `REPO_HAS_FILE` | Repo → File | — | Existing. |
| `FILE_HAS_SYMBOL` | File → Symbol | — | Existing. |
| `SERVICE_HAS_SYMBOL` | Service → Symbol | — | Existing. |
| `VAULT_HAS_NOTE` | Vault → Note | — | Containment for notes. |
| `NOTE_HAS_HEADING` | Note → Heading | — | Outline membership. |
| `NOTE_HAS_SECTION` | Note → Section | — | Section membership (every section belongs to exactly one note). |
| `HEADING_HAS_SECTION` | Heading → Section | — | A heading owns the section below it (until the next heading at depth ≤ its own). |
| `HEADING_PARENT` | Heading → Heading | — | Outline hierarchy (H2 child of H1, etc.). |
| `TAG_PARENT` | Tag → Tag | — | For nested `#a/b/c`. |

#### Cross-reference (the retrieval-relevant edges)

| Edge | From → To | Confidence range | Source |
|---|---|---|---|
| `WIKILINK` | Section → (Note \| Heading) | 0.0–1.0 | `[[Target]]` / `[[Target#Heading]]` in note body |
| `MARKDOWN_LINK` | Section → (Note \| File \| Symbol \| ExternalLink) | 0.0–1.0 | `[text](url)` |
| `TRANSCLUDES` | Section → (Note \| Heading \| Section) | 1.0 / 0.0 | `![[Target]]` embeds (Obsidian) |
| `TAGGED_WITH` | (Note \| Section) → Tag | 1.0 | Inline `#tag` or frontmatter `tags:` |
| `REFERENCES_CODE` | Section → Symbol | 0.0–1.0 | Cross-domain bridge (see §10) |
| `MENTIONS` | Section → Symbol | 0.0–0.5 | Low-confidence name match in prose (off by default) |

#### Project bundling

| Edge | From → To | Meaning |
|---|---|---|
| `PROJECT_INCLUDES_REPO` | Project → Repo | A project bundles a code repo. |
| `PROJECT_INCLUDES_VAULT` | Project → Vault | A project includes notes from a vault. |
| `PROJECT_INCLUDES_NOTE` | Project → Note | A project explicitly includes a note (cross-vault permitted). |
| `PROJECT_INCLUDES_SERVICE` | Project → Service | A project bundles a service. |

#### Backlinks — not stored

Backlinks are **derived**, not stored. They are the reverse traversal of `WIKILINK` / `TRANSCLUDES` / `MARKDOWN_LINK` / `REFERENCES_CODE`. Storing them duplicates writes and risks divergence. LadybugDB supports efficient reverse traversal already; the MCP `backlinks(uid)` tool runs a parameterised Cypher query at request time.

### 3.3 UID format for markdown nodes

```
vlt:{instance}:{path_hash}
note:{vault_uid}:{rel_path_hash}
head:{note_uid}:{slug_hash}:{line}
sec:{note_uid}:{start_line}:{content_hash_short}
tag:{vault_uid}:{name_hash}
proj:{instance}:{name_hash}
ext:{url_hash}
```

The `line` and `content_hash_short` qualifiers on `head:` / `sec:` make UIDs stable across edits that don't change the heading/section itself, while changing when the content does. This is the same stability/cache-bust tradeoff already made for `sym:`.

### 3.4 How markdown nodes coexist with code nodes

The same LadybugDB instance, same schema-versioned database, same PPR run. No partitioning. The graph is one graph. Three consequences:

1. **PPR scores are comparable across kinds.** A high-rank note and a high-rank symbol can be returned in the same ranked list.
2. **Cross-domain edges are first-class.** `REFERENCES_CODE` is no different from `CALLS` to the rank algorithm — both are weighted edges.
3. **Connectivity-density skew must be handled.** Symbols typically have many `CALLS` edges; notes have few `WIKILINK`s. Raw PPR would over-weight symbols. Mitigation: edge weights are normalised per source node (out-degree weighting), and PPR is run with per-kind reweighting (§6.4).

---

## 4. Parsing Pipeline

### 4.1 Library choices

| Concern | Crate | Why |
|---|---|---|
| Markdown AST | **`comrak`** (current ~0.30) | GFM-compatible, has a `wikilinks` extension, supports tables/tasks/footnotes, autolinks. ~2× slower than `pulldown-cmark` but feature-complete for Obsidian. |
| Frontmatter (YAML) | **`serde_yaml`** | Standard. Frontmatter must be parsed defensively — never reject a note for malformed YAML; record the parse error in `note.frontmatter_error` and continue with empty frontmatter. |
| File watching | **`notify`** (current ~6.x) | Cross-platform inotify/FSEvents/ReadDirectoryChangesW. Use `RecommendedWatcher` with debouncing (`notify-debouncer-mini`). |
| Slugification | hand-rolled, matching GitHub algorithm (lowercase, replace spaces with `-`, strip punctuation, deduplicate trailing `-`) | Obsidian uses GitHub-compatible slugs for `[[Note#Heading]]` resolution. Don't import a dependency for ~30 lines. |
| Wikilink parsing | manual regex + state machine over the section text | comrak's wikilinks extension covers `[[X]]` but not `![[X]]` transclusion or `[[X#Heading|display]]` reliably; do this step ourselves for full Obsidian compat. |

### 4.2 Parser output type

```rust
pub enum SourceKind {
    Code(Language),
    Markdown,
}

pub struct ParsedNote {
    pub path: String,
    pub title: String,
    pub frontmatter: serde_json::Value,
    pub frontmatter_error: Option<String>,
    pub note_kind: String,           // see §3.1
    pub content_hash: String,        // sha256 of the whole file
    pub headings: Vec<RawHeading>,
    pub sections: Vec<RawSection>,   // exactly one section per heading + optional preamble
    pub wikilinks: Vec<RawWikilink>,
    pub markdown_links: Vec<RawMdLink>,
    pub transclusions: Vec<RawTransclusion>,
    pub tags: Vec<RawTag>,           // inline + frontmatter
    pub code_blocks: Vec<RawCodeBlock>,  // feeds REFERENCES_CODE
    pub aliases: Vec<String>,        // from frontmatter `aliases:`
}

pub struct RawHeading { pub level: u8, pub text: String, pub slug: String, pub start_line: u32, pub end_line: u32 }
pub struct RawSection { pub heading_idx: Option<usize>, pub start_line: u32, pub end_line: u32, pub text: String }
pub struct RawWikilink { pub target: String, pub heading_anchor: Option<String>, pub display: Option<String>, pub section_idx: usize, pub line: u32 }
pub struct RawMdLink   { pub url: String, pub text: String, pub section_idx: usize, pub line: u32 }
pub struct RawTransclusion { pub target: String, pub heading_anchor: Option<String>, pub section_idx: usize, pub line: u32 }
pub struct RawTag      { pub name: String, pub source: TagSource, pub section_idx: Option<usize>, pub line: u32 }
pub struct RawCodeBlock { pub language: Option<String>, pub content: String, pub section_idx: usize, pub start_line: u32 }
```

Parallels `ParsedFile` for code — same `RawX` naming convention.

### 4.3 Pipeline

```
file → bytes → split frontmatter / body
   ├─ frontmatter: YAML parse (defensive) → serde_json::Value
   │     → extract aliases, tags (from `tags:`), note_kind (from `type:`), project (from `project:`)
   └─ body: comrak parse → AST walk
         → emit headings, sections, links, transclusions, tags, code blocks
            → assemble ParsedNote
```

### 4.4 Section-level chunking strategy

Sections are the **unit of retrieval**. A section spans from one heading to the next heading at depth ≤ the same depth. The preamble (text before the first heading) is a heading-less section.

Bound section size with two thresholds:

- **Soft target:** 256–512 tokens per section. Most real headings already produce sections in this range.
- **Hard cap:** 2,048 tokens. Above this, split on paragraph boundaries into multiple `Section` nodes that all share the same `heading_uid`. PPR and embeddings work better on similarly-sized chunks.

Sections that are smaller than 32 tokens get folded into the next sibling section to avoid one-line `Section` nodes dominating the BM25 index.

### 4.5 Incremental parsing

The file watcher delivers events. Pipeline:

```
event (Modify | Create | Remove, path)
   → if Remove: emit (file_path, ParsedNote::Deleted) to delta computer
   → if Modify | Create: read file → sha256 → if hash unchanged: drop event
                                     → else: parse → diff against prior ParsedNote
                                                  → emit delta
```

Debounce events to 200ms windows so a save (which can fire multiple events on macOS/Linux) collapses into one re-parse.

---

## 5. Graph Construction

### 5.1 How parsed nodes map to LadybugDB

DDL added in `init_schema()`. LadybugDB needs one `REL TABLE` per `(FROM, TO)` pair, so the logical edges from §3.2 flatten:

```sql
-- nodes
CREATE NODE TABLE Vault(uid STRING, root_path STRING, instance_id STRING, indexed_at TIMESTAMP, PRIMARY KEY(uid));
CREATE NODE TABLE Note(uid STRING, vault_uid STRING, path STRING, title STRING,
                      frontmatter STRING, note_kind STRING, content_hash STRING,
                      mtime TIMESTAMP, summary STRING, pagerank_score DOUBLE,
                      PRIMARY KEY(uid));
CREATE NODE TABLE Heading(uid STRING, note_uid STRING, level INT64, text STRING,
                          slug STRING, start_line INT64, end_line INT64,
                          content_hash STRING, PRIMARY KEY(uid));
CREATE NODE TABLE Section(uid STRING, note_uid STRING, heading_uid STRING,
                          start_line INT64, end_line INT64, text_hash STRING,
                          pagerank_score DOUBLE, PRIMARY KEY(uid));
CREATE NODE TABLE Tag(uid STRING, vault_uid STRING, name STRING, PRIMARY KEY(uid));
CREATE NODE TABLE Project(uid STRING, name STRING, summary STRING,
                          instance_id STRING, PRIMARY KEY(uid));
CREATE NODE TABLE ExternalLink(uid STRING, url STRING, host STRING, PRIMARY KEY(uid));

-- containment
CREATE REL TABLE VAULT_HAS_NOTE(FROM Vault TO Note);
CREATE REL TABLE NOTE_HAS_HEADING(FROM Note TO Heading);
CREATE REL TABLE NOTE_HAS_SECTION(FROM Note TO Section);
CREATE REL TABLE HEADING_HAS_SECTION(FROM Heading TO Section);
CREATE REL TABLE HEADING_PARENT(FROM Heading TO Heading);
CREATE REL TABLE TAG_PARENT(FROM Tag TO Tag);

-- cross-reference (per FROM/TO pair)
CREATE REL TABLE WIKILINK_TO_NOTE(FROM Section TO Note, confidence FLOAT, display STRING);
CREATE REL TABLE WIKILINK_TO_HEADING(FROM Section TO Heading, confidence FLOAT, display STRING);
CREATE REL TABLE MDLINK_TO_NOTE(FROM Section TO Note, confidence FLOAT, text STRING);
CREATE REL TABLE MDLINK_TO_FILE(FROM Section TO File, confidence FLOAT, text STRING);
CREATE REL TABLE MDLINK_TO_SYMBOL(FROM Section TO Symbol, confidence FLOAT, text STRING);
CREATE REL TABLE MDLINK_TO_EXT(FROM Section TO ExternalLink, text STRING);
CREATE REL TABLE TRANSCLUDES_NOTE(FROM Section TO Note);
CREATE REL TABLE TRANSCLUDES_HEADING(FROM Section TO Heading);
CREATE REL TABLE TRANSCLUDES_SECTION(FROM Section TO Section);
CREATE REL TABLE NOTE_TAGGED_WITH(FROM Note TO Tag);
CREATE REL TABLE SECTION_TAGGED_WITH(FROM Section TO Tag);
CREATE REL TABLE REFERENCES_CODE(FROM Section TO Symbol, confidence FLOAT, kind STRING);
CREATE REL TABLE MENTIONS(FROM Section TO Symbol, confidence FLOAT);

-- project bundling
CREATE REL TABLE PROJECT_INCLUDES_REPO(FROM Project TO Repo);
CREATE REL TABLE PROJECT_INCLUDES_VAULT(FROM Project TO Vault);
CREATE REL TABLE PROJECT_INCLUDES_NOTE(FROM Project TO Note);
CREATE REL TABLE PROJECT_INCLUDES_SERVICE(FROM Project TO Service);
```

Update `NODE_LABELS` / `EDGE_LABELS` in `nestweaver-schema/src/version.rs` correspondingly so the schema hash captures the extension.

### 5.2 Wikilink resolution (5-priority scheme)

For each `RawWikilink { target, heading_anchor }` in section `S` of note `N` in vault `V`:

```
1. PATH MATCH: if target contains '/', treat as path relative to V's root.
              Try V/{target}.md, V/{target}/index.md, V/{target}.
              If unique match → confidence 1.0.

2. UNIQUE TITLE MATCH: case-insensitive lookup in (Note.title ∪ aliases) within V.
              If exactly one match → confidence 1.0.

3. ALIAS MATCH: as 2, but the match is via frontmatter `aliases:` rather than title.
              If unique → confidence 0.7.

4. SAME-FOLDER MATCH: restrict 2/3 to notes in the same folder as N.
              If unique → confidence 0.5.

5. AMBIGUOUS: multiple candidates from steps 2–4 → emit one edge per candidate
              with confidence 1/N each.

UNRESOLVED: no candidates → emit a placeholder edge with
              target_uid = "unresolved-note:{slug_of_target}", confidence 0.0.
              This preserves the link in the graph; Obsidian users expect to see
              "this note would link to X if X existed."

HEADING ANCHOR (post-resolve): once the target note is identified, if
              `heading_anchor` is set, look it up in Heading.slug within that note.
              Found → emit WIKILINK_TO_HEADING (overrides WIKILINK_TO_NOTE for this edge).
              Missing → emit WIKILINK_TO_NOTE with partial=true, missing_anchor recorded.
```

Confidence values become edge weights in PPR. Unresolved edges (weight 0) are kept for visibility but don't propagate rank.

### 5.3 Deduplication of cross-vault references

A single instance can host multiple vaults. Notes with identical titles across vaults are not deduplicated — they are distinct `Note` nodes. Wikilink resolution scopes to the source note's vault by default. Cross-vault links are expressed explicitly via `[[vault-name:Note Title]]` (Obsidian's "vault link" syntax) and resolved against the target vault.

Tags are vault-scoped: `tag:vlt:work:a3f2...` and `tag:vlt:personal:a3f2...` are distinct nodes even if both are `#decision`. A cross-vault `Tag` aggregate can be added later if needed but is not the v1 default.

### 5.4 Cross-domain bridges

`REFERENCES_CODE` connects `Section` nodes to `Symbol` nodes. Three sources, in decreasing confidence (full details in §10):

1. Explicit `<!-- nw:ref sym:... -->` annotations — 1.0.
2. Symbol names appearing in fenced code blocks inside the section, scoped to repos that share a `Project` with the section's note — 0.6.
3. Backtick-quoted names in prose, same scoping — 0.4, off by default.

`MDLINK_TO_FILE` and `MDLINK_TO_SYMBOL` are produced when standard `[text](url)` URLs point to repository files (relative paths matching a known repo's file structure, or `file://` URLs).

---

## 6. Ranking & Retrieval — The Core Innovation

### 6.1 Why Personalized PageRank

The retrieval problem is: given a small set of seed nodes (symbols, notes, sections, tags — whatever the user/agent specifies), return a ranked list of all other nodes by **structural relevance**, where relevance respects both direct and multi-hop connectivity.

Naive options and why they're worse:

- **BFS/DFS by depth:** treats all neighbours equally; one-hop noise drowns out important two-hop signal.
- **Embedding similarity:** lexical/semantic, ignores explicit structure. Misses "the function called by the function described in this PRD."
- **Edge-count centrality:** popular ≠ relevant to *your* seeds.

**PPR** (Page & Brin '99, formalised by Haveliwala '02) is mathematically the stationary distribution of a random walker that, with probability `1−d`, teleports back to the seed set. It gives a smooth, principled, multi-hop notion of "structural distance" from the seeds. The research literature is unambiguous:

- TERAG: PPR over LLM-built graphs hits 80%+ accuracy at 3–11% of competing methods' token cost.
- HippoRAG: PPR is the hippocampal "associative memory" that pattern-completes across episodes — 10–30× cheaper multi-hop than CoT-RAG.
- Aider: PPR-ranked repo-map outperforms naive context inclusion on coding accuracy.

### 6.2 Precomputed PPR — the key performance lever

**Full PageRank** (no personalization) is computed once at index time and stored as a `pagerank_score` property on each node. Cost: O(iterations × edges), typically 20 iterations × few hundred thousand edges ≈ seconds.

**Personalized PageRank** is the per-query workload. Two strategies, both implemented:

**Strategy A — query-time PPR (cold):** for queries with novel seed sets, run PPR on the in-memory adjacency at query time. With <50K nodes (the design target), 20-iteration PPR runs in ~10–50ms. This is the existing implementation in `nestweaver-store/src/ranking.rs:221`.

**Strategy B — precomputed forward-push vectors (hot):** for common single-seed queries (every individual symbol, every individual note), precompute a sparse PPR vector via the forward-push algorithm (Andersen-Chung-Lang '06). Store as a sparse `HashMap<NodeUid, f32>` per seed, keeping only entries above a residual threshold (e.g. ε = 1e-4). Multi-seed queries are then a weighted sum of single-seed vectors (linearity of PPR) — O(seeds × nnz) per query, typically <1ms.

Storage cost: for 50K nodes with average sparsity ~200 entries per vector, that's ~10M float entries × 8 bytes ≈ 80MB. Acceptable. For 5K nodes, ~4MB. Fits in RAM trivially.

Strategy B is the source of the <100ms query target. The system starts with A and progressively materialises B for hot seeds (LRU eviction).

### 6.3 Incremental PPR updates

A file watcher event produces a delta: `(nodes_added, nodes_removed, edges_added, edges_removed)`. Recomputing full PPR is wasteful for a one-note edit.

**Algorithm (forward-push, RisGraph-style):**

```
on edge_added(u, v, w):
  for each seed s with a cached PPR vector P_s:
    residual_v += w * P_s[u] / out_degree(u)
    if residual_v > epsilon: enqueue v for push
  drain push queue (bounded by epsilon * max_iter)

on edge_removed: symmetric — recompute affected residuals, push.

on node_added(n): no-op until first edge touches n.

on node_removed(n): drop from caches; mark seeds whose vectors include n as dirty
                    (lazy recompute on next access).
```

Expected work per edge update: **O(1/ε)** amortised, which in practice is sub-millisecond for ε = 1e-4. RisGraph (SIGMOD '21) demonstrates millions of ops/sec on commodity hardware with this approach. We don't need that throughput — we need sub-100ms for a save event, which is comfortable headroom.

**Persistence:** PPR vectors and the full-PageRank scores are persisted to a sidecar file (`<db>.ppr-incremental.bin` using `bincode` or `rkyv` for fast load). Rebuilt on startup if the schema hash has changed (the existing `effective_schema_hash` mechanism detects this).

### 6.4 Unified PPR across code + docs

The same graph, the same PPR run. Two subtleties:

**Edge weighting.** All edges have a `weight` in `[0,1]`. Code edges use the existing confidence scoring (`SameFileExact = 0.95`, `ImportResolved = 0.90`, ...). Doc edges use the resolver confidences from §5.2. Cross-domain edges (`REFERENCES_CODE`) use their source-tier confidences (1.0 / 0.6 / 0.4). The PPR transition matrix is weight-normalised per source node.

**Kind-balancing.** Symbols have ~5–50× more edges than notes (functions call many things; notes wikilink to few). Without balancing, raw PPR over-weights symbols. Apply a per-kind multiplier `α_kind` to the personalization vector and a per-kind out-weight normaliser, calibrated so that the average PPR mass within each kind is comparable. Concretely, after edge weighting, scale each node's outbound weights by `1 / out_weight_sum(node)` — standard PPR normalisation — and additionally apply a kind-prior `α_kind` to the teleport vector. Defaults: `α_symbol = 1.0`, `α_section = 1.2`, `α_note = 1.0`. Tunable via config.

### 6.5 Hybrid retrieval: PPR + BM25 + embeddings

PPR is the primary signal. BM25 and embeddings are for **coverage** — surfacing nodes that aren't graph-connected to the seeds but are semantically relevant.

```
brain_context(seeds, token_budget):
  1. resolve seeds → seed_uids
  2. PPR rank → ranked_by_ppr  (primary)
  3. text query (extracted from seed names) → BM25 rank → ranked_by_bm25
  4. text query → embedding lookup → ranked_by_vector
  5. reciprocal-rank-fusion: score(node) = Σ_i 1/(k + rank_i(node))
     with k=60 (standard RRF), weighted: PPR 0.6, BM25 0.25, vector 0.15
  6. token-budgeted greedy selection (§6.6)
  7. enrich with metadata, signatures, snippets
```

The BM25 index lives in-process (the `tantivy` crate is the safest bet for production — it's a Rust Lucene-equivalent, file-backed, incremental). The vector index reuses the existing `EmbeddingIndex` in `nestweaver-store/src/search.rs`, extended to embed `Section` and `Note` content alongside `Symbol`.

### 6.6 Token budgeting

```
greedy_token_budgeted(ranked_nodes, budget):
  selected = []
  used = 0
  for each node in ranked_nodes (sorted by fused score desc):
    cost = render_cost(node)  // ~tokens needed to include this node
    if used + cost > budget: continue  // skip — try next, allow gaps
    selected.append(node)
    used += cost
  // After main pass: ensure kind diversity — if all picks are one kind,
  // and budget headroom remains, do a second pass biased to under-represented kinds.
  return selected
```

`render_cost(node)` depends on the node kind and what the output includes:

| Kind | Default render | Approx tokens |
|---|---|---|
| Symbol | `name | kind | file:line | signature` | 25 |
| Section | `note_title§heading | first 80 chars` | 60 |
| Note | `title | path | summary` | 40 |
| Tag | `#name | note_count` | 8 |
| Project | `name | summary | counts` | 50 |

`render_cost` is approximate (char-count / 4 + structural overhead). The MCP response includes a `token_count` field so the caller knows the actual cost.

### 6.7 Citing the literature

- **Token efficiency:** TERAG's 89–97% output-token reduction validates the design goal that a brain serving <500-token responses outperforms baselines serving 5K+ tokens for the same queries.
- **Multi-hop reasoning:** HippoRAG's 10–30× cost reduction on multi-hop questions is the regime where this system most outperforms naive RAG — and where a working engineer most needs help ("the decision in the meeting note about the auth refactor that changes the mobile contract").
- **Live updates:** RisGraph's sub-ms per-update PPR maintenance is the algorithmic basis for the §6.3 incremental updater.
- **Production validation:** Aider's repo-map ships PPR-ranked code context to LLMs in production and measurably improves coding accuracy.

---

## 7. MCP Server Interface

### 7.1 Implementation

The MCP server is implemented in `nestweaver-mcp` using **`rmcp`** (the official Rust MCP SDK, `rmcp` crate). Transport: stdio (default for Claude Desktop / Claude Code integration). SSE/HTTP optional behind a feature flag.

Launched via `nestweaver mcp serve [--db PATH] [--config PATH]`. Holds the `GraphStore` and PPR caches in memory for the lifetime of the process. Optionally launches the file watcher (`nestweaver mcp serve --watch`) so the brain stays live without a separate `nestweaver watch` daemon.

### 7.2 Tool catalogue

#### Read tools (Phase 1–2)

```jsonc
// brain_search — hybrid search across all node kinds
{
  "name": "brain_search",
  "description": "Search across symbols, notes, sections, and tags. Returns ranked results with relevance scores. Use when you need to find specific named things or topics.",
  "input": {
    "query": "string",
    "kinds": "string[]?",        // ["symbol","note","section","tag","project"]; default all
    "limit": "int?",             // default 20
    "scope": "string?"           // optional project name to scope to
  },
  "output": {
    "results": [{
      "uid": "string",
      "kind": "string",
      "name": "string",
      "snippet": "string",
      "location": "string",       // file:line or note#heading
      "score": "number"
    }],
    "token_count": "int"
  }
}

// brain_context — the primary tool. PPR over seeds, token-budgeted.
{
  "name": "brain_context",
  "description": "Get the most structurally relevant context for the given seeds, fit within a token budget. Use this BEFORE reading individual files — it returns ranked, structured context (symbols + note sections + cross-references) from across code and docs. Seeds can be symbol names, note titles, [[wikilink]] syntax, tags (#name), or UIDs.",
  "input": {
    "seeds": "string[]",
    "token_budget": "int?",      // default 4000
    "scope": "string?",          // "code"|"notes"|"unified"; default unified
    "project": "string?",        // restrict to a project bundle
    "include_bodies": "bool?"    // include section bodies vs. just headers; default false (cheaper)
  },
  "output": {
    "seeds_resolved": [{ "uid": "...", "name": "...", "kind": "..." }],
    "items": [{
      "uid": "string",
      "kind": "string",
      "name": "string",
      "location": "string",
      "relevance": "number",       // fused PPR + BM25 + vector score
      "body": "string?",           // only if include_bodies=true
      "summary": "string?"
    }],
    "cross_links": [{ "from": "uid", "to": "uid", "kind": "string", "confidence": "number" }],
    "token_count": "int",
    "truncated": "bool"
  }
}

// note_get — full note content
{
  "name": "note_get",
  "description": "Fetch a full note's content by title, [[wikilink]], or UID. Returns frontmatter, outline, body, outgoing links, tags. Use when brain_context indicates a specific note is highly relevant and you want its full content.",
  "input": {
    "id": "string",              // title | [[link]] | uid
    "include_body": "bool?",     // default true
    "include_backlinks": "bool?" // default false (separate cost)
  },
  "output": {
    "uid": "string",
    "title": "string",
    "path": "string",
    "frontmatter": "object",
    "note_kind": "string",
    "outline": [{ "level": "int", "text": "string", "slug": "string" }],
    "body": "string?",
    "outgoing_links": [{ "kind": "string", "to": "uid", "display": "string" }],
    "tags": "string[]",
    "backlinks": [...]?
  }
}

// note_section — one section's body, precision retrieval
{
  "name": "note_section",
  "description": "Fetch the body of a specific section by note + heading. Cheaper than note_get for targeted reads.",
  "input": { "note": "string", "heading": "string" },
  "output": { "uid": "string", "text": "string", "location": "string", "token_count": "int" }
}

// backlinks — what references this
{
  "name": "backlinks",
  "description": "Find everything that references the target: notes/sections that wikilink to it, sections that transclude it, sections that reference it as code, etc.",
  "input": { "target": "string", "limit": "int?" },
  "output": {
    "incoming": [{
      "from_uid": "string",
      "from_kind": "string",
      "from_name": "string",
      "edge_kind": "string",
      "confidence": "number",
      "snippet": "string"
    }]
  }
}

// projects_list — top-level inventory
{
  "name": "projects_list",
  "description": "List all known projects with their summaries and asset counts. Start here when you don't know which project the user is referring to.",
  "input": {},
  "output": {
    "projects": [{
      "name": "string",
      "summary": "string",
      "repos": "int",
      "vaults": "int",
      "notes": "int",
      "last_touched": "timestamp"
    }]
  }
}

// project_context — everything relevant to a project, token-budgeted
{
  "name": "project_context",
  "description": "Return a token-budgeted view of a project: key notes, key symbols, current status, recent activity, cross-references. Use when starting work on a project.",
  "input": { "project": "string", "token_budget": "int?" },
  "output": "same as brain_context"
}

// code_for — given a doc, what code implements it
{
  "name": "code_for",
  "description": "Given a note (e.g. a PRD or design doc), return the code symbols most likely to implement it, ranked by the strength of doc↔code links.",
  "input": { "note": "string", "limit": "int?" },
  "output": { "symbols": [{ "uid": "...", "name": "...", "location": "...", "confidence": "number" }] }
}

// note_for — given code, what docs describe it
{
  "name": "note_for",
  "description": "Given a code symbol, return the notes (PRDs, design docs, decisions) most likely to describe it.",
  "input": { "symbol": "string", "limit": "int?" },
  "output": { "notes": [{ "uid": "...", "title": "...", "location": "...", "confidence": "number" }] }
}

// existing code tools, kept and re-exposed
"symbol_get", "repo_map", "impact"
```

#### Write tools (Phase 3+)

```jsonc
// note_create — write back into the vault
{
  "name": "note_create",
  "description": "Create a new note in the vault. The brain indexes it immediately. Use for capturing decisions, meeting notes, or work logs.",
  "input": {
    "vault": "string?",          // default: configured primary vault
    "path": "string",            // relative to vault root, .md will be appended if missing
    "title": "string",
    "body": "string",
    "frontmatter": "object?",    // merged with sensible defaults (date, type)
    "tags": "string[]?"
  },
  "output": { "uid": "string", "path": "string" }
}

// note_link — add a wikilink (or other edge)
{
  "name": "note_link",
  "description": "Add a wikilink from one note's section to another note. Useful for connecting newly created notes.",
  "input": { "from_section": "string", "to_note": "string", "anchor": "string?" },
  "output": { "edge_uid": "string" }
}

// note_append — append to a note (e.g., add an item to a list)
{
  "name": "note_append",
  "description": "Append content to a specific section of a note. Use for incremental updates like adding to a TODO list or journal.",
  "input": { "note": "string", "heading": "string?", "content": "string" },
  "output": { "uid": "string", "new_section_uid": "string?" }
}
```

### 7.3 Response format conventions

- All responses are JSON with a `token_count` field so the agent can self-budget across multiple calls.
- `relevance` / `confidence` / `score` are always in `[0, 1]`.
- `location` is a stable, agent-readable string: `file:line` for code, `vault/path.md#heading` for notes.
- Truncation is explicit (`truncated: true`) — never silently drop.
- Errors return `{ "error": "...", "code": "not_found"|"ambiguous"|"invalid" }`.

### 7.4 Streaming

Large responses (e.g. `note_get` on a long doc) support MCP's streaming response capability — emit headings/outline first, then body chunks. For `brain_context`, prefer to keep responses small enough that streaming isn't needed (the whole point is token-budgeting). Streaming is available but not the default path.

### 7.5 Tool design heuristics

These tools will be read by Claude many thousands of times. Descriptions are as important as the schemas:

- Lead with **when to use**, not what it does.
- Always say what comes back, in plain terms.
- Mention costs when relevant ("cheaper than X").
- Cross-reference related tools ("after brain_context returns a note ranked high, use note_get").

Iterate descriptions based on real transcripts — if Claude reaches for the wrong tool, the description is at fault.

---

## 8. Performance Design

### 8.1 Performance targets

| Operation | Target | Mechanism |
|---|---|---|
| `brain_context` query | <100ms p95 | Precomputed PPR vectors + in-RAM adjacency + greedy budget selection |
| `brain_search` query | <50ms p95 | Tantivy BM25 + in-RAM vector index |
| Full reindex (1K-note vault) | <5s | Parallel parsing, batch DDL, one PPR run at the end |
| Incremental update (save event) | <100ms p95 | Forward-push PPR update + delta DDL |
| Memory footprint (50K nodes) | <500MB | Sparse PPR vectors, lazy section bodies, mmap'd LadybugDB |
| Startup time (warm cache) | <500ms | Persisted PPR sidecars loaded directly |
| Startup time (cold) | <5s for 1K notes / <30s for 50K | Triggered only on schema-hash change |

### 8.2 Where the time goes

**Query path (the hot path):**
```
MCP request parse              ~1ms
seed resolution (UID/name)     1–5ms (BM25 + indexed lookup)
PPR vector load + sum          1–10ms (precomputed, sparse-vector arithmetic)
hybrid score fusion            1–5ms
token-budget greedy selection  1–3ms
result enrichment (Cypher)     5–30ms (batched read against LadybugDB)
JSON serialize                 1–5ms
                              ──────
                               15–60ms typical, 100ms p99
```

**Indexing path:**
```
walk + read I/O                bounded by disk; rarely the limit
parse (per file)               <5ms code, <2ms markdown
resolve wikilinks              O(links) hashmap lookups; fast
batch insert                   LadybugDB batches are ~50K rows/sec
PPR full compute               O(20 × |edges|); ~1s per 100K edges
embedding compute              network-bound to inference endpoint;
                              skipped by default for v1, behind a flag
```

### 8.3 In-memory graph for sub-ms traversal

The store keeps two representations:

1. **LadybugDB on disk** — source of truth, durable, queryable via Cypher.
2. **In-RAM adjacency** — `Vec<u32>` CSR (compressed sparse row) over node indices, built at startup, mutated incrementally. Used for PPR and traversal.

The in-RAM representation is rebuilt on startup from the DB. Cost: O(|edges|) reads, typically <1s for 100K edges.

### 8.4 Lazy loading of section bodies

`Section.text` is large (potentially a couple of KB each, 50K sections × 2KB = 100MB). Strategy:

- v1: store inline in the DB. Simple, fits in RAM.
- v2 (if needed): store `text_hash` in DB, load the body on demand from the original file using `(path, start_line, end_line)`. The cost is one open-file-read per request that needs the body. Acceptable since `brain_context` defaults to `include_bodies=false` and only headlines are needed for ranking.

### 8.5 Concurrency

- One `GraphStore` per process (LadybugDB is single-writer).
- Reads via independent `lbug::Connection`s — already the existing pattern.
- The MCP server uses `tokio` for concurrent tool handling; each tool call gets its own connection.
- PPR cache reads are lock-free (immutable after computation); writes (incremental updates) take a brief write lock.
- The file watcher runs on a dedicated thread, feeds a `tokio::mpsc` channel to the indexer task.

---

## 9. Data Ingestion & Keeping the Brain Current

**This section is the most important in the document.** Every other piece of the design is wasted effort if data ingestion is hard. The vast majority of "knowledge graph for LLMs" projects fail not on the algorithm but on the seam between the user's existing notes/code and the system. Users don't tolerate friction here — not config files, not migrations, not "first run a setup script." The system has to feel like Spotlight: you point at a folder, it works, and you forget it exists.

This section is the contract the system makes with the user: **one command in, never think about it again.**

### 9.1 The personas this must serve

The design must hold for all four simultaneously. If any persona has to read documentation to get going, the design has failed for everyone.

| Persona | Starting state | What "works" looks like |
|---|---|---|
| **Power user** | 5,000-note Obsidian vault built over years. Wikilinks, frontmatter, custom plugins, nested tag hierarchies, `.obsidianignore`. | `brain add ~/vault` indexes it cleanly in <30s. Respects `.obsidianignore`. Handles their messy frontmatter without complaint. File watcher keeps it current as they write. |
| **Developer** | Several code repos under `~/dev/`, a separate `~/Documents/notes/` Obsidian vault. Wants them queryable together. | `brain add ~/dev/myproject` and `brain add ~/Documents/notes`. The two sources unify into one graph. PRDs cross-link to code via `REFERENCES_CODE` (§10). |
| **New user** | Three markdown files in `~/Desktop/notes/`. Wants to see if this is useful before committing. | `brain add ~/Desktop/notes` works on a directory with no `.obsidian/` and no `.git/`. No config required. Result is visible in seconds. |
| **Team / shared docs** | A shared `docs/` folder synced via Dropbox, iCloud, or a git submodule. Multiple contributors edit it. | `brain add ~/Dropbox/team-docs` indexes it. File watcher picks up changes from other team members' edits as their sync client lands files locally. No coordination needed. |

The same command path works for all four. Source auto-detection (§9.4) makes the difference invisible.

### 9.2 Zero-friction onboarding — the one-command contract

The bar is:

```sh
$ nestweaver brain add ~/Documents/my-vault
Detected Obsidian vault: 847 notes, 12 tags, 2,340 wikilinks
Indexing... done in 4.2s
Brain ready: 847 notes, 4,219 edges, 51 cross-references
Watching for changes. (Stop watching with: nestweaver brain pause)
```

That output is end-to-end: zero config files written by the user, zero questions asked, zero plugins installed, immediate feedback on what was found. Compare:

| Tool | Onboarding model | NestWeaver inheritance |
|---|---|---|
| **macOS Spotlight** | Indexes silently in the background. User never thinks about it. | The "set and forget" file watcher (§9.5). |
| **Raycast** | Install → launch → works. No setup. | The single-command install path. |
| **Obsidian** | Point at a folder. The vault is just a folder. | The "any directory is a valid source" stance (§9.4). |
| **ripgrep** | Has zero startup cost. You just run it. | The CLI-first ethos — every feature reachable in one command. |

#### Implicit defaults that remove configuration

- **Database location:** `~/.local/share/nestweaver/brain.lbug` (XDG-compliant on Linux; `~/Library/Application Support/nestweaver/brain.lbug` on macOS). Never asks the user.
- **Single unified DB across sources.** All sources merge into one brain. Multi-DB is an advanced opt-in (`--db <path>`), not the default. This is the difference between "a tool" and "your brain."
- **No `nestweaver init`.** First `brain add` creates the DB if absent. There is no separate setup step.
- **No required config file.** Sources live in `~/.config/nestweaver/sources.toml` (XDG) but are written by `brain add`; the user never opens it.
- **File watcher starts automatically.** A daemon (`nestweaverd` or `nestweaver brain watch` running in the background) is launched on first `brain add` and registered for autostart via the platform's mechanism (launchd plist on macOS, systemd --user unit on Linux). On Windows, a startup shortcut. If the user doesn't want autostart, `--no-watch` opts out — but the default is on.

#### Conversational onboarding via MCP

Claude must also be able to add sources without dropping to the shell. Expose:

```jsonc
{
  "name": "brain_add_source",
  "description": "Add a directory as a source for the brain. Auto-detects the source type (Obsidian vault, code repo, plain markdown folder). Indexes immediately and starts watching for changes. Use when the user mentions notes, vaults, or repos that aren't yet indexed.",
  "input": {
    "path": "string",            // absolute or ~-relative path
    "name": "string?",           // friendly name; defaults to dir name
    "watch": "bool?"             // default true
  },
  "output": {
    "source_uid": "string",
    "kind": "string",            // "obsidian"|"markdown"|"repo"|"mixed"
    "indexed": { "notes": "int", "files": "int", "symbols": "int", "edges": "int" },
    "elapsed_ms": "int"
  }
}
```

Companion read tools:

- `brain_list_sources()` — return the current source registry.
- `brain_source_status(name_or_path)` — health, last index time, watcher state.
- `brain_remove_source(name_or_path, drop_data?)` — unregister; optionally drop from graph.
- `brain_refresh(name_or_path?)` — manual reindex trigger.

Conversational example:

> User: "Hey, I keep my notes at `~/notes`, can you index them?"
> Claude: *calls `brain_add_source({ path: "~/notes" })`*
> Server: `{ kind: "obsidian", indexed: { notes: 847, ... }, elapsed_ms: 4200 }`
> Claude: "Added your vault — 847 notes, 4,219 edges, ready to query. I'll keep it current as you write."

### 9.3 Real-time vs scheduled vs manual: what's the right model

**The answer is event-driven real-time as the default, background catch-up on startup, and manual refresh as an escape hatch. Never cron.** Each model has a specific job; together they cover every realistic update pattern.

#### Tier 1 — Real-time file watching (always on, the primary update path)

This is how Obsidian, VS Code, and Spotlight stay current. The kernel tells us when files change; we don't poll.

| Platform | Mechanism | Cost when idle |
|---|---|---|
| macOS | FSEvents (via `notify` crate) | Near zero — kernel callback |
| Linux | inotify (via `notify`) | Near zero |
| Windows | ReadDirectoryChangesW (via `notify`) | Near zero |

End-to-end latency from "user hits Cmd-S" to "brain updated and queryable":

```
file save                    0ms
FSEvents callback            <5ms (kernel)
debouncer collects burst     200ms (intentional)
read file + hash             <5ms typical
short-circuit if hash same   immediate exit
parse delta                  <10ms typical
apply DB delta               <20ms (one note's nodes/edges)
incremental PPR update       <10ms (forward-push, §6.3)
                             ──────
total: ~250ms p50, <500ms p95
```

The 200ms debounce is the deliberate component — IDE saves often fire 2–5 events in rapid succession (write tempfile, rename, atime update on Linux). Without debouncing the indexer re-runs that many times.

```rust
// engine sketch — single watcher serving N sources
let (tx, rx) = tokio::sync::mpsc::channel(4096);

let mut debouncer = notify_debouncer_mini::new_debouncer(
    Duration::from_millis(200),
    move |events: DebounceEventResult| {
        if let Ok(events) = events {
            for e in events { let _ = tx.blocking_send(e); }
        }
    },
)?;

for src in registry.sources()? {
    debouncer.watcher().watch(&src.root, RecursiveMode::Recursive)?;
}

tokio::spawn(async move {
    while let Some(ev) = rx.recv().await {
        if let Err(e) = ingest.handle_event(ev).await {
            tracing::warn!(?e, "ingest event failed; continuing");
        }
    }
});
```

The watcher task uses `tracing::warn!` and never crashes the daemon. Individual file failures are isolated.

#### Tier 2 — Background catch-up (automatic, invisible)

The watcher only runs when the daemon is running. Between sessions — laptop sleep, daemon crash, system reboot, edits made on another machine that synced in — the brain falls out of date. Tier 2 closes the gap.

**On daemon startup:**
1. Walk each registered source.
2. For each file, compare `mtime` against `last_indexed_mtime` recorded in the DB.
3. For files where `mtime > last_indexed_mtime`: enqueue for reindex.
4. For files in DB but not on disk: enqueue for removal.
5. Drain the queue on a background worker — the daemon is fully usable for queries during this catch-up (stale-read-ok semantics, results just don't include not-yet-indexed updates).

For unchanged files (`mtime` matches and `content_hash` unchanged): **skip entirely.** This is the difference between "fast" and "instant" startup on a 5K-note vault — at most a stat() per file (~1ms × 5K = 5s if cold, way less when the OS dir cache is warm).

```rust
// startup catch-up sketch
async fn catch_up(store: &GraphStore, registry: &SourceRegistry) -> Result<CatchupReport> {
    let mut report = CatchupReport::default();
    for source in registry.sources()? {
        let on_disk = scan_source(&source).await?;            // (path, mtime) pairs
        let in_db = store.indexed_files_for_source(&source.uid)?;
        for (path, mtime) in on_disk {
            match in_db.get(&path) {
                Some(prev_mtime) if *prev_mtime >= mtime => { report.unchanged += 1; }
                Some(_) => { ingest.reindex_file(&path).await?; report.updated += 1; }
                None    => { ingest.index_file(&path).await?; report.added += 1; }
            }
        }
        for path in in_db.keys() {
            if !on_disk.contains_key(path) { ingest.remove_file(path).await?; report.removed += 1; }
        }
    }
    Ok(report)
}
```

The report is logged and shown in `brain status`:

```
$ nestweaver brain status
Sources:
  ~/Documents/my-vault     obsidian   847 notes   last update 12s ago   ✓ watching
  ~/dev/myproject          repo       2,143 files last update 3m ago    ✓ watching

Last catch-up: +3 added, 12 updated, 0 removed (832 unchanged)
Daemon: running (pid 4127, uptime 2d 4h)
```

#### Tier 3 — Manual refresh (escape hatch)

There are situations the watcher legitimately misses:

- `git pull` or branch switch updates many files at once and some watchers throttle / drop events under burst.
- A bulk paste from another tool that bypasses normal save semantics.
- Cloud-sync clients (Dropbox, iCloud) sometimes deliver files via paths that don't fire watcher events on all platforms.
- Manually fixed corruption.

Surface as:

- CLI: `nestweaver brain refresh [path]` — reindex one source or all. Reuses the catch-up path; `--force` ignores hash and reindexes everything.
- MCP: `brain_refresh(source?)` tool. Claude calls this when the user mentions doing a git pull or large change.
- Auto-trigger heuristic: if the watcher reports an event burst above N files in T seconds, the daemon proactively schedules a full catch-up on that source. This converts the "I just did a git pull and you missed half of it" failure mode into a delay rather than missing data.

#### Tier 4 — Never cron

Cron / scheduled tasks are the wrong primitive:

- Too frequent → wasted I/O when nothing changed.
- Too infrequent → stale brain.
- Schedule drift across machines, time zones, sleeps.
- Failures are silent (cron logs end up in `/var/mail` nobody reads).
- No event semantics — you reindex everything every N minutes regardless of whether anything happened.

The combination of Tier 1 (event-driven) + Tier 2 (startup catch-up) + Tier 3 (manual escape hatch) is strictly better. The system never asks "when should I check?" — it knows.

### 9.4 Source types and auto-detection

`brain add <path>` runs detection in priority order. The first match wins; the result is recorded in the source registry.

```rust
enum SourceKind {
    Obsidian { vault_root: PathBuf, app_json: Option<ObsidianConfig> },
    Repo { root: PathBuf, vcs: Vcs /* Git, Hg, ... */ },
    Mixed { has_obsidian: bool, has_repo: bool },
    Markdown { root: PathBuf },        // plain folder with .md files
    Empty,                             // nothing recognisable
}

fn detect(path: &Path) -> SourceKind {
    let has_obsidian = path.join(".obsidian").is_dir();
    let has_repo     = path.join(".git").is_dir();      // extensible: .hg, .svn
    let has_md       = walk_shallow(path).any(|p| ext(p) == "md");

    match (has_obsidian, has_repo, has_md) {
        (true,  true,  _   ) => SourceKind::Mixed { has_obsidian: true, has_repo: true },
        (true,  false, _   ) => SourceKind::Obsidian { vault_root: path.into(), app_json: read_app_json(path) },
        (false, true,  _   ) => SourceKind::Repo { root: path.into(), vcs: Vcs::Git },
        (false, false, true) => SourceKind::Markdown { root: path.into() },
        (false, false, false) => SourceKind::Empty,
    }
}
```

#### Per-kind behaviour

**Obsidian vault** (`.obsidian/` present):
- Read `.obsidian/app.json` for vault-level config (attachments folder, daily-notes folder).
- Read `.obsidian/community-plugins.json` to detect known plugins (Dataview, Tasks, Templater) that affect parsing — set hints, don't fail without them.
- Honor `.obsidianignore` (Obsidian's gitignore-equivalent).
- Use Obsidian's wikilink resolution rules (canonical title → first-match in vault, etc.).
- Vault name: take from the directory name; allow `--name` override.

**Git repository** (`.git/` present):
- Use the existing code parser (tree-sitter for JS/TS/Java/Go/Python).
- Honor `.gitignore` for skipping (existing `SKIP_DIRS` constant in `nestweaver-engine/src/index.rs` becomes a fallback; `.gitignore` is the source of truth).
- Repo URL: `git config --get remote.origin.url` if available; else `file://<path>`.
- Branch detection: store the indexed branch in the `Repo` node; on branch switch, reindex.

**Mixed** (both `.obsidian/` and `.git/`):
- Two parsers run side-by-side over the same root.
- One `Repo` node + one `Vault` node, both with the same root path.
- Cross-domain linking (`REFERENCES_CODE`) auto-enabled for sections in the vault referring to symbols in the repo. The most powerful default — a monorepo with docs and code becomes a single unified brain entry.

**Plain markdown folder** (no markers, just `.md` files):
- Treated as a lightweight vault. No `.obsidianignore` to read; falls back to `.gitignore` if present, then to hard defaults.
- Wikilinks still parsed (Obsidian-compatible syntax is becoming a de facto standard).
- Title resolution: H1 → filename → fallback.

**Empty / unrecognised:**
- Report and exit, don't silently create an empty source. Error message includes "no `.md` files, no `.git/`, no `.obsidian/` found — did you mean a different path?"

#### Future kinds (later phases, same detector)

| Kind | Detection | Notes |
|---|---|---|
| **Notion export** | `Export-*.zip` or `*-Notion-export/` with characteristic UUID filenames | Convert to markdown on import, run as markdown source. |
| **Confluence export** | XML structure + `entities.xml` | Similar — normalise to markdown. |
| **Logseq graph** | `logseq/` directory + page/`.md` files | Compatible enough with Obsidian that the markdown source handles it once Logseq-flavoured block refs are recognised. |
| **Apple Notes** | API-driven, not a folder | Out of scope for v1; needs a different connector model. |

### 9.5 What happens on first run — the full UX walkthrough

```
$ brew install nestweaver
==> Pouring nestweaver--0.2.0.arm64_sonoma.bottle.tar.gz
==> nestweaver installed

$ nestweaver brain add ~/Documents/my-vault
Detecting source type...
  Found .obsidian/ → Obsidian vault
  Reading .obsidian/app.json (attachments → "assets", daily-notes → "Journal/Daily")
  Reading .obsidianignore (3 patterns excluded)
  Found 847 markdown files (excluded 12)

Indexing...
  [████████████████████████████████████] 847/847  3.8s
  Headings:    4,891    Sections: 6,204
  Wikilinks:   2,340    Resolved: 2,287 (98%)   Unresolved: 53
  Tags:        12       Tagged notes: 312
  Frontmatter: 421 notes with frontmatter

Computing PageRank...                                       0.4s
Building search index (BM25)...                             0.2s

Brain ready.
  Database: ~/.local/share/nestweaver/brain.lbug
  Total: 847 notes · 11,107 nodes · 8,544 edges

Watching for changes. The brain stays current automatically.
  Daemon registered with launchd (com.nestweaver.daemon)

Try: nestweaver brain context "your most-edited topic"
     nestweaver mcp serve   # then add to Claude Desktop config
```

Every line of that output is information the user actually needs. Nothing is jargon. The numbers prove the system found their stuff. The next-step suggestions are concrete.

After this, the user never thinks about indexing again. Edit a note in Obsidian — saved, indexed, queryable within half a second. Reboot the laptop — the daemon restarts automatically, catches up on anything that changed while off (e.g. iCloud-synced edits from their phone). Switch git branches — the daemon detects the burst and reindexes the repo.

#### Add-vault progress at scale

For very large vaults (>10K notes) or repos (>100K files), show a phase breakdown rather than a single bar:

```
Indexing ~/dev/monorepo...
  Walking filesystem...          14,203 files found       0.4s
  Parsing source files...        [████████░░░░] 8,041/14,203
  Resolving cross-file refs...   pending
  Computing PageRank...          pending
```

Phase visibility prevents the perception of a hang during long full indexes. Bars use `indicatif`.

### 9.6 Multi-source management

Sources are first-class objects in a registry at `~/.config/nestweaver/sources.toml`:

```toml
# Auto-managed by `nestweaver brain` commands. Edit at your own risk.
schema_version = 1

[[sources]]
uid          = "vlt:default:a3f2..."
name         = "my-vault"
path         = "/home/user/Documents/my-vault"
kind         = "obsidian"
added_at     = 2026-05-23T10:14:00Z
last_indexed = 2026-05-23T10:14:04Z
watching     = true
attachments_dir = "assets"

[[sources]]
uid       = "repo:default:b81c..."
name      = "myproject"
path      = "/home/user/dev/myproject"
kind      = "repo"
added_at  = 2026-05-23T11:02:00Z
last_indexed = 2026-05-23T11:02:18Z
watching  = true
branch    = "main"
remote    = "git@github.com:user/myproject.git"
```

CLI surface (full inventory):

| Command | Effect |
|---|---|
| `nestweaver brain add <path> [--name N] [--no-watch]` | Detect, register, index, start watching. |
| `nestweaver brain list` | Pretty-printed source table with status. |
| `nestweaver brain status [name]` | Detailed health: last index, file counts, watcher state, recent errors. |
| `nestweaver brain remove <name> [--keep-data]` | Stop watching, unregister, optionally drop nodes/edges from graph. |
| `nestweaver brain refresh [name] [--force]` | Manual reindex of one or all sources. `--force` bypasses hash short-circuit. |
| `nestweaver brain pause [name]` | Stop watcher without removing source. |
| `nestweaver brain resume [name]` | Restart watcher. |
| `nestweaver brain rename <old> <new>` | Rename a source. |
| `nestweaver brain move <name> <new-path>` | Update path after a directory move (auto-triggered when vault moved is detected by `.obsidian/` fingerprint — see §9.7). |

All operations are also exposed via MCP (Claude can do everything from a conversation).

**Graph unification:** all sources merge into one graph in one database. Project nodes (§3.1, §10) span sources. The brain's job is to **dissolve the source boundary** at query time — Claude asks about "device pairing" and gets sections from the vault next to symbols from the repo, ranked together, with no awareness that they came from different sources.

### 9.7 Edge cases that quietly kill adoption

The list below is the actual difference between "tried it once, gave up" and "use it daily." Every item is a real failure mode that has killed adoption for similar tools, addressed with a specific, tested handling rule.

| Case | Default behaviour | Why this default |
|---|---|---|
| **Symlinks** | Follow them, with cycle detection (track `(dev, inode)` pairs visited; abort a path on revisit). | Real vaults symlink across folders (shared `assets/`, work/personal vault links, dotfile setups). Refusing to follow breaks them. |
| **`.gitignore` / `.obsidianignore`** | Honor strictly. `.gitignore` parsed via the existing crate the code parser already uses. `.obsidianignore` is gitignore-syntax. | Indexing `node_modules` will single-handedly tank the experience. Trust the user's existing ignore files. |
| **Hidden files (`.*`)** | Skip by default. Override with `--include-hidden`. | Avoids indexing `.DS_Store`, `.obsidian/workspace.json`, etc. |
| **Binary files** | Skip by content sniff (first 8KB, treat as binary if >30% non-text). Record `path` and `mtime` so the watcher doesn't re-sniff every event. | PDFs, images, audio attached to notes shouldn't crash the parser or pollute the graph. v2: an `Attachment` node with metadata-only indexing. |
| **Huge files (>5MB)** | Skip; log a warning. Configurable via `--max-file-size`. | Multi-MB markdown is almost always machine-generated (logs pasted in). Parsing them takes seconds and tanks ranking. |
| **Permission errors** | Skip the file; log at WARN; continue. Never abort the index for one unreadable file. | Synced-drive permission glitches and dotfile symlinks regularly cause this. |
| **Encoding** | Assume UTF-8. On non-UTF-8 decode error, attempt UTF-16 with BOM, then Windows-1252. Failing all three, skip with WARN. | Hand-edited notes from Windows users sometimes land as Windows-1252; auto-fall-through covers them silently. |
| **Vault moved on disk** | Detect via `.obsidian/` directory's stable contents (a fingerprint over `app.json` + plugins manifest + creation timestamp). On startup, if a registered source's path is missing but a matching fingerprint exists elsewhere on common search paths (under `~/Documents`, `~/Dropbox`, `~/iCloud Drive/...`), auto-update the source path. Otherwise prompt. | Vault moves are common (drive renames, sync client migration). Manual reconfiguration here is the #1 churn reason. |
| **Multiple vaults** | First-class. Each is its own source, its own `Vault` node, its own wikilink scope. Cross-vault links are explicit (`[[vault-name:Note]]`). | Power users routinely keep work and personal vaults separate. |
| **Atomic-save tools** (most editors) | Watch reports `Remove` then `Create` instead of `Modify`. The debouncer + content-hash short-circuit collapses this back to a single update with no flicker in the graph. | Without this, every save would briefly delete the note from the graph. |
| **Cloud-sync mid-write** | Dropbox/iCloud write `filename (conflicted copy).md` on conflicts. These are indexed normally — they're real files. The user resolves the conflict the same way they always have. | Don't try to be clever about conflicts; the cloud client owns that problem. |
| **Daemon already running** | Second `brain add` invocation talks to the running daemon via a Unix socket at `~/.local/state/nestweaver/daemon.sock`; doesn't try to start a second process. | Two daemons watching the same paths produce duplicate events and corruption. |
| **Daemon not running** | CLI commands start it (idempotent), wait for socket ready, proceed. | Don't make the user think about daemons. |
| **Out-of-disk during index** | Detect on first `ENOSPC`, abort cleanly, leave existing graph unchanged. | Half-written indexes are worse than no index. |
| **Power loss during write** | LadybugDB writes are transactional. PPR sidecars are written via tempfile + atomic rename. | Recover by reopening, rerun catch-up. |
| **Schema upgrade between versions** | Detect via `effective_schema_hash` mismatch on startup. Prompt once: "Schema changed, full reindex (~30s). Continue?" Default yes after 5s. | One-time pain, transparent rationale. Never silent data loss. |
| **Watcher backpressure** | The mpsc channel has bounded capacity (4096 events). On full, log WARN and trigger a scheduled catch-up rather than blocking the watcher thread. | A 10K-file `find . -exec touch` won't lock up the daemon. |
| **Watch limit exhausted (Linux)** | Catch `ENOSPC` from inotify init; print clear remediation (`echo fs.inotify.max_user_watches=524288 | sudo tee /etc/sysctl.d/99-nestweaver.conf`); fall back to polling-mode watcher (less efficient but functional). | Linux's default 8192 watch limit is the single most common surprise for users with large trees. |

These rules are encoded as tests. The integration suite includes a fixture vault that exercises every row in this table — that's the gate for shipping.

### 9.8 Delta computation and incremental PPR

(Carried forward from the previous edition of this section.)

Per changed note:

```rust
let new_parsed = parse_note(&path, &source)?;

// short-circuit: if the file's content hash hasn't changed, nothing to do.
if store.note_content_hash(&note_uid)? == Some(new_parsed.content_hash.clone()) {
    return Ok(IngestOutcome::Unchanged);
}

let old_nodes = store.headings_and_sections_of(&note_uid)?;
let old_edges = store.outgoing_edges_of_note(&note_uid)?;
let new_nodes = derive_nodes(&new_parsed);
let new_edges = derive_edges(&new_parsed);

let node_delta = diff_by_uid(&old_nodes, &new_nodes);
let edge_delta = diff_by_endpoints_and_kind(&old_edges, &new_edges);

apply_delta(&store, &node_delta, &edge_delta)?;
ppr_updater.apply_delta(&edge_delta)?;
Ok(IngestOutcome::Updated { added: node_delta.added.len(), removed: node_delta.removed.len() })
```

Diff is by UID for nodes (UIDs are stable across edits when content unchanged) and by `(src_uid, tgt_uid, edge_kind)` tuple for edges.

The PPR updater holds `HashMap<SeedUid, SparseVec<NodeUid, f32>>` of cached vectors. For each edge delta:

1. For each cached seed: apply forward-push residuals to the affected nodes (§6.3).
2. If a cached seed's vector mass changes by > 1% (configurable), recompute it fully on the next access (lazy refresh).
3. Persist updated vectors at most once per 5s (batched).

Full-PageRank `pagerank_score` per node is marked stale and recomputed in a background pass every N edits or every M seconds — whichever comes first.

### 9.9 Persistence layout

```
~/.local/share/nestweaver/                  (XDG_DATA_HOME)
  brain.lbug                                LadybugDB primary store
  brain.lbug.pagerank.json                  full PageRank snapshot
  brain.lbug.ppr-incremental.bin            sparse precomputed PPR vectors (rkyv)
  brain.lbug.tantivy/                       BM25 index directory
  brain.lbug.embeddings.bin                 vector index
  brain.lbug.manifests.json                 parsed manifests (existing)

~/.config/nestweaver/                       (XDG_CONFIG_HOME)
  sources.toml                              source registry
  config.toml                               optional user overrides (rare)

~/.local/state/nestweaver/                  (XDG_STATE_HOME)
  daemon.sock                               Unix socket for CLI↔daemon RPC
  daemon.pid                                pid file
  daemon.log                                rolling log (last 7 days)

~/Library/LaunchAgents/com.nestweaver.daemon.plist   (macOS autostart)
~/.config/systemd/user/nestweaver.service             (Linux autostart)
```

| Artefact | Format | When written |
|---|---|---|
| Graph data | LadybugDB on disk | Every write (transactional) |
| Full PageRank cache | `<db>.pagerank.json` | After full recompute |
| Sparse PPR vectors | `<db>.ppr-incremental.bin` (`bincode` / `rkyv`) | Batched, every 5s with edits |
| BM25 index | Tantivy index directory `<db>.tantivy/` | Incremental on text change |
| Embedding vectors | `<db>.embeddings.bin` | On embedding compute |
| Source registry | `~/.config/nestweaver/sources.toml` | On every `brain add/remove/rename` |

On startup, validate `effective_schema_hash` against the DB. If unchanged → load all sidecars and start serving. If changed → drop sidecars, trigger a full reindex against the current source registry (one-time prompt as noted in §9.7).

### 9.10 The "it just works" acceptance test

The shipping bar is: **a user who has never read a NestWeaver doc must be able to follow this sequence without help:**

1. Install via package manager: `brew install nestweaver` or `cargo install nestweaver`.
2. Run one command: `nestweaver brain add ~/my-vault` (or any folder path).
3. Never run another command. The brain stays current automatically across saves, reboots, branch switches, and sync events.
4. Configure Claude Desktop / Claude Code with one MCP server entry, then ask Claude about their projects and get accurate, current answers.

If any of those steps requires reading documentation, editing a config file, or remembering to re-index — the design has failed and must be fixed before ship. The acceptance test is run on a fresh user (or fresh VM with no prior NestWeaver state) at each release.

### 9.11 What we explicitly do not ask the user to do

The list of removed friction is as important as the list of features. Each row below is something competing tools require that NestWeaver must not:

- ❌ Edit a YAML/TOML/JSON config to declare sources.
- ❌ Run a one-time `init` or `migrate` command.
- ❌ Choose between multiple index modes ("incremental" vs "full") on the command line.
- ❌ Install a separate plugin for their editor.
- ❌ Pick a database path or worry about where data lives.
- ❌ Manually restart anything when they edit a note.
- ❌ Set up a cron job, systemd timer, or scheduled task.
- ❌ Re-add their vault after `brain` is upgraded.
- ❌ Pre-compute embeddings before queries work.
- ❌ Provide an API key for basic operation (embeddings are optional and only used if configured).
- ❌ Understand what PPR, BM25, RRF, or a knowledge graph is.
- ❌ Open the database in a separate tool to inspect anything — `brain status`, `brain list`, and the MCP read tools cover all observability.

If any of these creeps back in during development, treat it as a release-blocker — onboarding is the load-bearing wall.

---

## 10. Cross-Domain Linking

### 10.1 The bridge edges

A design doc says "the auth service handles token refresh." A code repo has an `AuthService.refreshToken` method. The brain's job is to know these are linked, so that when the agent asks for context on either, the other appears.

`REFERENCES_CODE` is the bridge. Source confidences:

| Source | Confidence | Recall |
|---|---|---|
| Explicit `<!-- nw:ref sym:repo:...:abc:42 -->` annotation | 1.0 | Low (must be hand-added) |
| Symbol name found inside a fenced code block in the section, scoped to repos linked via Project | 0.6 | Medium |
| Backtick-quoted symbol name in prose, same scoping | 0.4 | High (but noisy) |
| Section title or heading that exact-matches a unique symbol name in linked repos | 0.7 | Medium |

The default v1 config enables sources 1, 2, and 4. Source 3 is behind a `--enable-prose-mentions` flag — it's the noisiest and benefits from a user opting in once they understand the tradeoff.

### 10.2 Scoping via Project

Without scoping, source 2 would link a section mentioning `User` to every `User` class in every indexed repo. The fix: only emit `REFERENCES_CODE` edges when the section's note belongs to a `Project` that also includes the symbol's repo. This is why `Project` is a first-class node.

A simpler heuristic for users without explicit Projects: same vault ↔ same instance — only link to repos indexed under the same instance ID as the vault. Configurable.

### 10.3 Bi-directionality

Edges are directed (`Section → Symbol`), but PPR over the unified graph propagates relevance in both directions because PPR runs on the undirected closure (the existing `load_ppr_graph` adds reverse edges). So a query seeded with a symbol naturally surfaces the doc sections that reference it, ranked by closeness.

### 10.4 Resolved example

```
[Note: "Device Pairing PRD"]
  └─ HEADING_HAS_SECTION
      └─ [Section: "§3 Error Handling"]
          ├─ TAGGED_WITH → [Tag: #project/pairing]
          ├─ WIKILINK_TO_NOTE → [Note: "Pairing State Machine"]
          ├─ REFERENCES_CODE → [Symbol: PairingService.handleTimeout]  (confidence 0.6, source: code block)
          └─ REFERENCES_CODE → [Symbol: ErrorMapper.toUserMessage]      (confidence 0.4, source: prose)

User asks: "context for working on pairing timeouts"
Agent: brain_context(seeds=["pairing timeout"], token_budget=3000)

Server:
  → seed resolution: BM25 match → both Section §3 AND Symbol handleTimeout score high
  → unified PPR with both seeds
  → top results include:
       Symbol PairingService.handleTimeout (0.91)
       Section "Device Pairing PRD §3 Error Handling" (0.84)
       Symbol PairingService.retryWithBackoff (0.72)  ← reached via CALLS edge
       Note "Pairing State Machine" (0.66)              ← reached via WIKILINK
       Symbol ErrorMapper.toUserMessage (0.51)
       Section "Meeting 2026-04-15 §pairing decisions" (0.43)  ← reached via project membership
  → response: ranked list, ~1200 tokens, mix of code and docs
```

This is the workload that justifies the whole system. No naive RAG returns this picture in one call.

---

## 11. Implementation Phases

### Phase 1 — Markdown brain MVP (1 week)

**Goal:** Index a vault, query it from the CLI.

| Day | Work |
|---|---|
| 1 | Schema: add `Vault`, `Note`, `Heading`, `Section`, `Tag`, `Project` structs + UID helpers + `NODE_LABELS`/`EDGE_LABELS` updates. |
| 2 | Markdown parser: comrak integration, frontmatter (serde_yaml), heading/section extraction, wikilink/tag/code-block extraction. |
| 3 | Wikilink resolver (5-priority) + tag handling + transclusion handling. |
| 4 | Store: DDL for new tables, per-kind insert/read methods, `index_markdown_directory` in engine. |
| 5 | **Generalise `load_ppr_graph` with `GraphScope`.** Unified PPR over code + notes. |
| 6 | CLI: `index-vault`, `note`, `notes`, `wikilinks`, `tag`. |
| 7 | Tests, end-to-end smoke on a real vault. |

**Exit criteria:** `nestweaver index-vault ~/Documents/notes && nestweaver context "device pairing"` returns ranked notes + headings.

### Phase 2 — MCP server + hybrid retrieval (1 week)

**Goal:** Claude can use the brain.

| Day | Work |
|---|---|
| 1 | `nestweaver-mcp` against `rmcp`: server scaffold, stdio transport. |
| 2 | Implement `brain_search`, `note_get`, `note_section`, `backlinks`. |
| 3 | Tantivy BM25 integration for `brain_search` and seed resolution. |
| 4 | Hybrid scoring (PPR + BM25 + vector RRF). Token budgeting. |
| 5 | Implement `brain_context` end-to-end with token-budgeted output. |
| 6 | `projects_list`, `project_context`, `symbol_get`, `repo_map`. |
| 7 | Wire into Claude Code; iterate tool descriptions against real transcripts. |

**Exit criteria:** Claude responds to a project question with `brain_context` instead of file reads, in <100ms, within token budget.

### Phase 3 — Cross-domain + incremental + writes (2 weeks)

**Goal:** Live, bi-directional, writable brain.

| Week | Work |
|---|---|
| 3 | `REFERENCES_CODE` resolver (explicit + code blocks + heading-name match). `code_for` / `note_for` MCP tools. `Project` bundling in instance config. |
| 4 | Incremental indexer: `notify` file watcher, delta computer, forward-push PPR updater. Persisted sparse PPR vectors. Write tools: `note_create`, `note_link`, `note_append`. |

**Exit criteria:** Save a note → brain updates in <100ms. Claude can create notes and have them indexed immediately.

### Phase 4 — Additional connectors and polish (ongoing)

| Stream | Notes |
|---|---|
| `Connector` trait refactor | Extract `CodeConnector` and `MarkdownConnector` cleanly. Required before adding more connectors. |
| Linear connector | Issues, comments, projects → nodes. Cross-link issue → code via title/branch matching. |
| Slack connector | Threads referenced by canonical URL → nodes. High-value for capturing "the conversation where we decided X." |
| Calendar connector | Meetings → MeetingNote nodes pre-populated from invites. |
| Section-body on-demand loading | Migrate from inline to `(path, start_line, end_line)` lazy load. Only if RAM/DB size becomes a concern. |
| Multi-instance UX | The existing instance config already supports it; the brain should let Claude target an instance via tool args. |
| `suggest-projects` | Extend `suggest-links` to propose project bundles from co-mentioned terms. |

### Risk register

| Risk | Mitigation |
|---|---|
| LadybugDB schema migrations | v1: detect via `effective_schema_hash`, full reindex on mismatch. Migrations are a v2 problem. |
| PPR skew between high-degree symbols and sparse notes | Kind-prior multipliers in §6.4. Tune on real vaults. |
| Wikilink ambiguity in large vaults | Resolver degrades to `1/N` confidence across candidates; the MCP `note_get` tool exposes "did you mean" candidates. |
| Frontmatter inconsistency | Parse defensively; never reject a note. |
| Bad tool descriptions → Claude picks wrong tool | Iterate against transcripts; ship instrumented logging from day one. |
| Tantivy bloat | Tantivy indices are large. Acceptable for v1 (<500MB on 50K notes). If problematic later, fall back to simpler trigram index. |
| Comrak performance on huge notes | Sections >2KB are already split; very large notes (>1MB) are rare and worth a hard cap with a warning. |

---

## 12. Research References

- Yin et al. (2025). **TERAG: Token-Efficient Graph-Based Retrieval-Augmented Generation.** arXiv:2509.18667. https://arxiv.org/abs/2509.18667
  *PPR-based graph retrieval; 80%+ accuracy at 3–11% token cost; 89–97% output-token reduction.*

- Gutiérrez, B. J. et al. (2024). **HippoRAG: Neurobiologically Inspired Long-Term Memory for Large Language Models.** NeurIPS 2024. https://arxiv.org/abs/2405.14831
  *Neocortex/hippocampus framing; KG + PPR for associative memory; 10–30× cheaper multi-hop reasoning vs. CoT-RAG.*

- Feng, G. et al. (2021). **RisGraph: A Real-Time Streaming System for Evolving Graphs with Analytics.** SIGMOD 2021. https://dl.acm.org/doi/10.1145/3448016.3457263
  *Sub-millisecond per-update PPR maintenance; millions of ops/sec on commodity hardware. Algorithmic basis for incremental PPR.*

- Andersen, R., Chung, F., Lang, K. (2006). **Local Graph Partitioning using PageRank Vectors.** FOCS 2006. https://dl.acm.org/doi/10.1109/FOCS.2006.44
  *Forward-push algorithm for sparse personalized PageRank — the algorithm behind the precomputed PPR vectors in §6.2 and the incremental updater in §6.3.*

- Haveliwala, T. H. (2002). **Topic-Sensitive PageRank.** WWW 2002. https://dl.acm.org/doi/10.1145/511446.511513
  *Formal introduction of Personalized PageRank.*

- Page, L., Brin, S., Motwani, R., Winograd, T. (1999). **The PageRank Citation Ranking: Bringing Order to the Web.** Stanford InfoLab. http://ilpubs.stanford.edu/422/
  *Original PageRank paper. Required reading for the random-walk-with-restart intuition.*

- Gauthier, P. **Aider's repo-map: Improving GPT-4's codebase understanding with ranked tags maps.** https://aider.chat/docs/repomap.html
  *Production validation: PPR-ranked code context outperforms naive context inclusion on coding accuracy.*

- Karpathy, A. **My wiki-based personal knowledge system.** (informal post / talk)
  *Structured-markdown context reduces tokens 20–40× vs. raw chat history, but breaks at scale without graph retrieval — motivates the need for the Project Brain.*

- LadybugDB (lbug). https://crates.io/crates/lbug
  *The underlying property-graph store. Cypher-compatible, single-writer, multi-reader.*

- comrak. https://crates.io/crates/comrak — GFM-compatible Markdown parser used for note parsing.
- rmcp. https://crates.io/crates/rmcp — Official Rust MCP SDK used for the brain server.
- notify / notify-debouncer-mini. https://crates.io/crates/notify — File watcher for incremental indexing.
- tantivy. https://crates.io/crates/tantivy — Lucene-class BM25 index for hybrid retrieval.

---

## Appendix A — Manifest Analysis: What NestWeaver Already Parses

Before extending NestWeaver with new ingest paths, take stock of what it already understands. The codebase contains a **manifest parsing layer** that has direct relevance to how project bundling and cross-domain linking should work in the brain extension.

### A.1 Two different "manifest" concepts in the codebase

The word "manifest" is overloaded in this repo. Two unrelated things share the name:

| Concept | Where | Purpose |
|---|---|---|
| **`ManifestInfo`** (code dependency manifest) | `crates/nestweaver-engine/src/manifest.rs` (328 lines) | Parse `package.json` / `go.mod` / `Cargo.toml` / `pyproject.toml` / `requirements.txt` to extract `{ package_name, dependencies }`. Used as a high-confidence signal for cross-repo link suggestions. |
| **`Manifest`** (snapshot descriptor) | `crates/nestweaver-engine/src/snapshot.rs` | Metadata file (`manifest.json`) inside a portable graph snapshot, describing what's inside the snapshot. Unrelated to source-code manifests. |

When you see "manifest" in commits, docs, or code, look at the import path: `crate::manifest::ManifestInfo` is the dependency parser, anything under `snapshot.rs` is the snapshot artefact. The brain extension only interacts with the first one.

### A.2 What `ManifestInfo` does today

```rust
pub struct ManifestInfo {
    pub package_name: Option<String>,
    pub dependencies: Vec<String>,
}
```

`parse_manifest(repo_path)` tries each known format in order, first match wins:

| Format | Detection | What's extracted |
|---|---|---|
| `package.json` (npm/yarn/pnpm) | File exists at repo root | `name` field; keys of `dependencies`, `devDependencies`, `peerDependencies` |
| `go.mod` | File exists at repo root | `module` line as package name; entries inside `require (...)` block |
| `Cargo.toml` | File exists at repo root | `package.name`; keys of `dependencies`, `dev-dependencies`, `build-dependencies` |
| `pyproject.toml` | File exists at repo root | `project.name`; PEP 508 names from `project.dependencies` array |
| `requirements.txt` | File exists at repo root | No package name; one dependency per line, name extracted up to first non-identifier char |

Storage: serialised as `HashMap<repo_uid, ManifestInfo>` to a JSON sidecar `<db>.manifests.json` alongside the LadybugDB file. Written once per `nestweaver index` invocation. The sidecar is loaded by `suggest-links` to detect cross-repo dependencies.

### A.3 How manifests are used

The single consumer is `suggest_links()` in `nestweaver-engine/src/suggest.rs:235`. The flow:

```
for each pair (repo_a, repo_b):
  if manifest[a].dependencies contains manifest[b].package_name:
    emit SuggestedLink {
      from: repo_a, to: repo_b,
      link_type: SharedImport,
      confidence: High,
      description: "Depends on {pkg_name} (from manifest)"
    }
```

This is what makes `nestweaver suggest-links` propose `[[links]]` entries for the instance config based on actual package boundaries (npm `@myorg/foo`, Go modules, Rust crates) rather than fuzzy heuristics. It's a high-precision signal because it reads what the build tools believe.

A secondary, lower-confidence signal in the same file (IDF-filtered name matching) supplements manifests when they're missing or external.

### A.4 Relevance to the brain extension

**Verdict: keep, extend, generalise.** Manifests are not deprecated by the brain extension — they remain the single best source of signal for cross-repo connectivity. But the concept generalises:

| Source kind | "Manifest" equivalent | What to extract | Signal use |
|---|---|---|---|
| Code repo | `package.json` etc. (existing) | `package_name`, `dependencies` | Cross-repo `SharedImport` links (existing behaviour) |
| Obsidian vault | `.obsidian/app.json` + `.obsidian/community-plugins.json` | Vault name, attachments folder, plugins (Dataview, Templater, ...) | Source self-description; plugin-aware parsing hints (e.g. recognise Dataview queries as code blocks) |
| Plain markdown folder | `README.md` H1 + folder name | Folder identity | Lightweight self-description |
| Project bundle (§3.1) | `[[projects]]` block in `nestweaver-instance.toml` | Project name, member repos/vaults/notes, entry points | Already exists for code; extends to vaults trivially |
| Notion / Confluence export (future) | Their export metadata files | Workspace name, page tree | Same role as code manifests for that domain |

**Recommendation for the brain extension:**

1. **Don't rename or refactor `ManifestInfo`**. It already does what it does well. Adding new source kinds adds parallel structs, not a generalisation.
2. **Introduce `VaultManifest`** alongside `ManifestInfo` in `nestweaver-engine/src/manifest.rs` (or in a new `src/vault_manifest.rs`). It carries the Obsidian config bits (`attachments_dir`, `daily_notes_dir`, `plugins`, `aliases_enabled`, etc.) so the markdown parser can honour vault-level settings.
3. **Extend `<db>.manifests.json`** to a tagged-union format: `{ "code": HashMap<repo_uid, ManifestInfo>, "vault": HashMap<vault_uid, VaultManifest> }`. Backwards-compatibility: any existing sidecar (which is `HashMap<repo_uid, ManifestInfo>` directly) is upgraded on first read by detecting the missing wrapper and re-wrapping under `code`. Old DBs keep working.
4. **`suggest_links` becomes manifest-source-aware.** When suggesting project bundles, it can now also match vault tags / project frontmatter against repo names. Same pattern, broader inputs.

The brain extension does **not** introduce new manifest *formats* (no `brainfile.toml`, no `nestweaver.yaml` in every source). The principle from §9 holds: zero new config files required from the user. Manifests are read from files the user already maintains.

---

## Appendix B — Performance Validation: Real-World Stress Test

The body of this document makes specific performance claims (<100ms query, <5s reindex, <100ms incremental, <500MB memory at 50K nodes, sub-ms PPR updates). Before committing to those numbers, this appendix audits each against what is actually known versus what is extrapolated.

### B.1 Methodological honesty up front

**There are no benchmarks in this repo.** `grep -rn "bench\|benchmark\|criterion"` returns zero hits in `Cargo.toml`s and zero hits outside doc/test prose. No `criterion` dev-dependency, no `benches/` directory, no recorded timings in commit messages.

Every performance number in the architecture body is one of:

- **Extrapolated from algorithmic complexity** (e.g. "PPR is O(iters × edges)") — gives an order of magnitude, not a guarantee on a specific machine.
- **Cited from external research** (RisGraph throughput, TERAG token reduction) — measured on different hardware against different graphs, may not transfer.
- **Estimated from common knowledge of the underlying library** (LadybugDB write speed, comrak parse rate) — informed guess, not measured here.

This appendix flags each claim with a confidence tier:

- **PROVEN** — measured in this codebase or in a directly comparable codebase. (Currently empty set.)
- **PLAUSIBLE** — math and external benchmarks support it; budget has headroom; would be surprised if wrong.
- **SUSPECT** — sensitive to implementation details we haven't validated; could easily be 2–10× worse than claimed; must benchmark before shipping.
- **UNPROVEN** — we have no basis to commit to this number; benchmark first.

### B.2 Claim-by-claim audit

#### Claim 1: File watcher cost — "near zero CPU when idle" on 5K+ files

**Verdict: PLAUSIBLE for macOS / Windows. SUSPECT on Linux at scale.**

Reality check:

- **macOS FSEvents.** Path-based, not file-based. Watching `~/vault` recursively is one subscription regardless of how many files are inside. Zero per-file cost. Idle CPU truly is ~0%.
- **Windows `ReadDirectoryChangesW`.** Directory-based. One handle per watched directory. A vault with 200 subdirs = 200 handles. Idle cost is negligible but the OS has limits on watch handles per process (10K+, far above our needs).
- **Linux inotify.** **File- and directory-level.** Each watch consumes a slot in `fs.inotify.max_user_watches` (default **8192 system-wide per user** on most distros). A vault with 5K notes spread across 200 dirs uses ~200 directory watches (one per dir), not 5K — because inotify on directories reports events for files inside. **But:** `notify` crate's `RecursiveMode::Recursive` does walk the tree and add a watch per directory. 200 is fine; a vault with thousands of subdirs (rare but possible) plus other apps' watches could hit the limit.
- **The architecture body already acknowledges this** in §9.7 with the `ENOSPC` remediation. That's correct and necessary.

**Cloud sync (Dropbox/iCloud) is the real risk, not file count.** When Dropbox syncs 200 changed files in a 10s burst, the watcher receives 200+ events (often 2–3 per file: write tempfile, rename, attribute update). With `notify-debouncer-mini` set to 200ms, those collapse per-path; but if 200 distinct paths each fire 3 events, the debouncer still emits 200 events at the end of the window. The downstream pipeline must handle that burst — that's why §9.7 includes the "burst threshold → schedule catch-up" auto-trigger and bounded-mpsc backpressure.

**Required validation before ship:**

- [ ] Watch a 5K-note vault on Linux with stock inotify limit; confirm no `ENOSPC`.
- [ ] Trigger a 500-file Dropbox sync; measure event-burst handling, debouncer behaviour, memory growth during the burst.
- [ ] Confirm idle CPU on macOS / Linux / Windows over 1h with no edits.
- [ ] Validate `notify-debouncer-mini` actually deduplicates per-path (vs. emitting all events).

#### Claim 2: In-memory graph size — "<500MB for 50K nodes"

**Verdict: SUSPECT.** Memory math depends entirely on whether section text and embeddings are stored inline.

Math:

| Storage component | Per-node size | At 50K nodes |
|---|---|---|
| Node struct (id, name, kind, parent_uid, file_path, line, content_hash) — strings + integers | ~250 bytes | ~12 MB |
| Section text inline (avg 512 chars) | ~512 bytes | ~25 MB |
| Section text @ 2KB hard cap if long | up to ~2 KB | up to 100 MB |
| Embedding vector (384-dim f32) | ~1.5 KB | ~75 MB |
| Embedding vector (1536-dim f32, e.g. OpenAI) | ~6 KB | ~300 MB |
| Frontmatter JSON per Note (subset of nodes) | ~200 bytes × 0.2 | ~2 MB |
| **Sparse PPR vectors** — N seeds × avg 200 entries × (uid_str + f32) | ~2 KB / seed | depends on cached seeds |

If we cache PPR vectors for the top 10% of nodes (5K seeds) at 2 KB each → **10 MB**. For all 50K nodes → **100 MB**.

**Realistic totals:**

| Configuration | RAM estimate (50K nodes) | Comment |
|---|---|---|
| Lean: structure + line refs (no inline text, no embeddings, 5K cached PPR seeds) | ~25–50 MB | Fits comfortably. The <500MB claim holds. |
| Default: + inline section text | ~75–150 MB | Still well under 500 MB. |
| Embeddings (384-dim) on every node + section | ~150–250 MB | Within budget. |
| Embeddings (1536-dim, OpenAI) on every node + section + section text inline + all-node PPR cache | ~600–800 MB | **Blows the 500 MB budget.** |

**Implications for the design:**

- §8.4's "v1: store section text inline" is fine for 50K. **The architecture body should be amended: at 50K nodes the inline+1536-dim configuration breaks the budget.** Either cap embeddings at 384-dim, or move section text to lazy-load before scaling past ~30K.
- The "<500MB" claim is **conditional on configuration**, not guaranteed. Documentation must be honest about this.
- LadybugDB's mmap'd files don't count against RSS the same way as heap — physical memory pressure is OS-managed.

**Required validation:**

- [ ] Build a synthetic 50K-node graph; measure RSS after full load in three configurations: lean, default, with-embeddings.
- [ ] Measure RSS after 1h of incremental updates to confirm no leak.
- [ ] Measure peak RSS during reindex of a 50K-node vault.

#### Claim 3: PPR computation time — "<100ms for 50K-node graph at query time"

**Verdict: PLAUSIBLE for precomputed lookup; SUSPECT for cold compute on largest graphs.**

The architecture body distinguishes two paths:

- **Strategy A (cold, full PPR at query time).** For 50K nodes, 200K edges, 20 iterations: ~4M scalar ops per iteration × 20 = 80M ops. At ~10 ns/op realistic Rust SIMD-free code, that's ~800 ms. **The 10–50 ms estimate in §6.2 is optimistic by ~10×.** A more honest range for cold PPR on 50K nodes is **200ms–1s** depending on edge density and convergence.
- **Strategy B (precomputed sparse vector lookup + linear-combination across multi-seed).** Sparse-vector add: for ε=1e-4 vectors averaging 200 nonzeros, one lookup + add is sub-millisecond. Multi-seed (say 5 seeds) is still <5 ms. **The <100 ms total query budget is comfortable for Strategy B.**

**Implication:** the <100 ms p95 query target depends on having precomputed PPR vectors for the *resolved* seeds. For novel seeds we fall back to Strategy A and the budget is blown.

**Mitigation already in the design:** §6.2 already specifies LRU materialisation of hot seeds. What's missing is a clear policy for what happens when a query lands on a cold seed — does the user wait 800 ms or does the system return a Strategy-A approximation (e.g. fewer iterations or local-only forward push)?

**Required validation:**

- [ ] Cold PPR benchmark on synthetic graphs: 5K, 50K, 100K nodes. Measure iteration time. Confirm convergence iteration count.
- [ ] Sparse PPR vector lookup + combine benchmark for k seeds where k ∈ {1, 5, 20}.
- [ ] End-to-end query latency under hot vs cold seed conditions.

#### Claim 4: Incremental PPR — "O(1) per update in practice"

**Verdict: UNPROVEN. The O(1) is amortised over many updates with bounded residual ε, not a per-update guarantee.**

What the literature actually says:

- **Andersen–Chung–Lang forward-push** is `O(1/ε)` *per push*, with bounded total work across a connected component. With ε = 1e-4, that's up to 10⁴ push operations per update in the worst case — milliseconds, not microseconds.
- **RisGraph's million-ops/sec claims** apply to a specific subset of updates (uniform random edge insertions) and are *amortised*. Pathological updates (edges to high-degree nodes that ripple through many cached vectors) cost more.
- For our use case: each cached PPR vector that touches the affected nodes needs a forward-push update. If we cache 1000 hot seeds and the modified edge propagates to all of them, that's 1000 × push-work per edge change.

**Realistic per-save-event cost (one note edited, ~10 edges changed):**

| Cached seeds | Per-edge push cost | Total update budget |
|---|---|---|
| 100 hot seeds, low fanout | ~10 μs × 10 edges × 100 = ~10 ms | well under 100 ms |
| 1,000 cached seeds, moderate fanout | ~50 μs × 10 × 1000 = ~500 ms | **misses the <100 ms target** |
| All 50K seeds cached | unworkable — drop to lazy refresh + full periodic recompute |

**The cache size becomes the load-bearing parameter, not the algorithm.** A small cache (top-100 hot seeds) keeps updates fast but cold queries fall through. A large cache makes queries always-hot but kills update latency.

**Mitigation:** the design needs a clearer policy. Recommendation:
- Cache top-N PPR vectors by query frequency (LFU), N=200 to start.
- On edge change: synchronously push for cached seeds whose vector overlaps; mark others as *stale* (lazy recompute on next access).
- Mark the full pagerank_score stale; recompute in background every 60 s or every 1000 edits.

**Required validation:**

- [ ] Benchmark forward-push update at different ε values, different cache sizes, different graph shapes.
- [ ] Measure latency P50/P95/P99 of incremental updates under realistic note-edit patterns.
- [ ] Test the worst case: an edge change to a high-PageRank node, e.g. a heavily-linked hub note.

#### Claim 5: Disk I/O on every file save — "writes are transactional"

**Verdict: PLAUSIBLE for graph writes; SUSPECT for combined sidecar fsync cost.**

LadybugDB is built on `lbug` 0.16.1. The docs and source aren't in this repo, but the existing `nestweaver-store` already uses it in production for code indexing — graph writes happen on every `nestweaver index` without issue. The transactional model is the right one; per-save fsync cost on SSD is ~5–50 ms depending on what the txn commits.

The risk is **combined fsync amplification**: a single note save triggers (a) LadybugDB graph txn fsync, (b) PPR sidecar update (batched, 5s), (c) Tantivy BM25 commit (segment-based, deferred), (d) source-registry write if structure changed (rare). If all of these synchronously fsync, the latency budget blows out.

**Mitigation already in design:** the architecture body says PPR sidecar is batched every 5s and Tantivy commits are deferred. As long as that holds, only (a) is on the save-event hot path. **But the "<100 ms p95 save→queryable" target implies fsync(graph.lbug) under 50 ms, which depends on the disk.** On NVMe SSD this is comfortable. On spinning rust (rare for modern devs) it's a nope.

**Edge case worth calling out:** when the user closes the laptop lid mid-batch, the 5s-batched PPR vectors haven't been persisted. On wake, the daemon must detect this and either replay deltas from the LadybugDB log (LadybugDB has a WAL) or trigger a full PPR recompute. The simpler design: on graceful shutdown flush all sidecars; on crash recovery, recompute. The architecture body doesn't currently specify this — it should.

**Required validation:**

- [ ] Measure fsync latency for typical note save (single graph txn) on NVMe SSD.
- [ ] Measure on USB-stick / network-mounted home dir (Linux NFS scenario for some corp setups).
- [ ] Confirm Tantivy and LadybugDB writes are interleavable without lock contention.

#### Claim 6: Startup time — "<500 ms warm, <5 s cold for 1K notes / <30 s for 50K"

**Verdict: PLAUSIBLE for warm with the right serialisation format. UNPROVEN for cold at 50K.**

Warm startup loads:

- LadybugDB on disk → typically mmap'd, near-instant attach (~1–10 ms).
- Full PageRank JSON sidecar at 50K nodes × ~80 bytes/entry = ~4 MB JSON. `serde_json` parses ~50–200 MB/s → ~20–80 ms.
- Sparse PPR vectors in bincode/rkyv. **rkyv is zero-copy** — load is essentially mmap + validate, fits in <100 ms even at 80 MB. **bincode** requires full deserialise, ~200–500 ms at the same size.
- Tantivy index — also mmap-friendly, fast attach.
- Build in-memory adjacency from DB: `MATCH (a:Symbol)-[:CALLS]->(b:Symbol)` and similar across all edge types. **This is the unknown.** For 200K edges, LadybugDB's Cypher query throughput is the bottleneck. Existing code in `ranking.rs` already does this for the code-only graph; it works but its timing isn't measured.

**Recommendation:** use rkyv (not bincode) for PPR sidecar. Warm start <500 ms is plausible.

Cold startup at 50K nodes:

- Full reindex of 50K markdown sections from scratch.
- Parse: comrak at ~5K docs/sec → ~10 s.
- DB batch insert: ~50K rows/sec for LadybugDB batched writes → ~5–10 s for all nodes + edges.
- Full PageRank: ~1 s.
- BM25 index build: Tantivy at ~50K docs/min → ~60 s for sections.
- **Total cold cost realistically 60–120 s on 50K notes.**

The "<30 s for 50K" claim in the body is **optimistic by 2–4×**. Should be amended to "<2 minutes for 50K cold reindex." 1K notes at <5 s is correct.

**Required validation:**

- [ ] Cold reindex benchmark on synthetic 1K, 10K, 50K vaults.
- [ ] Warm startup benchmark with all sidecars present.
- [ ] Compare rkyv vs bincode for PPR vector load time.

#### Claim 7: Background indexing doesn't block queries

**Verdict: UNPROVEN. Depends on LadybugDB's concurrency model, which isn't documented here.**

The architecture body (§8.5) states: "One `GraphStore` per process (LadybugDB is single-writer). Reads via independent `lbug::Connection`s — already the existing pattern."

What this depends on:

- **If LadybugDB has MVCC** (multi-version concurrency control), readers see a consistent snapshot during writes and there's no blocking. Queries during catch-up just see the pre-catch-up state. Good.
- **If LadybugDB uses lock-based concurrency** with reader/writer locks, write transactions block readers for the duration of the txn. Per-write that's milliseconds — fine — but a batch insert of thousands of edges during catch-up could pause queries for seconds.

The existing code (`compute_pagerank` in `ranking.rs`) holds a `Mutex` over the pagerank cache — that's app-level, not DB-level. The DB-level behaviour is unknown without testing.

**Mitigation regardless of which model:** batch incremental updates in transactions sized to ~100 nodes/edges per txn (small commits), so the worst-case reader pause is bounded to a few ms.

**Required validation:**

- [ ] Test: run a `brain_context` query loop in one thread while another thread does an incremental update loop. Measure read latency P99.
- [ ] Test: same loop while catch-up reindex runs in background. Measure read latency P99.
- [ ] If reads pause unacceptably: investigate LadybugDB's tuning options or fall back to txn-size limiting.

### B.3 Summary table — claim by claim

| Claim | Body says | Verdict | Required action |
|---|---|---|---|
| File watcher idle CPU | "near zero" | PLAUSIBLE (macOS/Win); SUSPECT (Linux at scale) | Validate inotify watch count; test Dropbox-sync burst handling |
| Memory at 50K nodes | "<500 MB" | SUSPECT — depends on embedding dim and inline section text | Amend body: condition on configuration |
| Query latency p95 | "<100 ms" | PLAUSIBLE for precomputed PPR; SUSPECT for cold | Define cold-seed fallback policy |
| PPR cold compute (50K nodes) | "10–50 ms" implied | OPTIMISTIC by ~10× — realistic 200 ms–1 s | Amend body or accelerate with SIMD/parallel |
| Incremental PPR per update | "O(1)" | UNPROVEN — depends on cache size | Document cache-size policy; benchmark worst case |
| Save→queryable latency | "<500 ms p95" | PLAUSIBLE on NVMe; SUSPECT on slower disks / network home dirs | Measure fsync, document hardware assumption |
| Warm startup | "<500 ms" | PLAUSIBLE with rkyv | Choose rkyv over bincode; benchmark |
| Cold reindex (50K) | "<30 s" | OPTIMISTIC — realistic 60–120 s | Amend body to <2 min |
| Background indexing doesn't block | implied | UNPROVEN — depends on LadybugDB concurrency model | Concurrency benchmark before ship |

### B.4 Recommended pre-ship benchmark suite

Before any of the perf numbers in this doc are quoted to users (CLI help text, marketing copy, blog posts), the following benchmarks should land:

1. **Synthetic vault generator** — create 1K, 5K, 10K, 50K-note vaults with realistic distributions (heading depth, link density, tag count, section size). Reuse as test fixtures.
2. **Cold reindex bench** — `criterion`-driven, per vault size. Records p50/p95/p99 wall time and peak RSS.
3. **Warm startup bench** — load existing DB + sidecars from disk, time to first query-ready.
4. **Query latency bench** — `brain_context` with seed sets of size {1, 5, 20}, on cold vs hot seeds. p50/p95/p99.
5. **Incremental update bench** — simulate a note edit (modify N sections, M links). Measure save→queryable latency.
6. **Sustained-load bench** — 1h of mixed query + edit traffic; track latency drift and RSS growth.
7. **Concurrency bench** — readers vs writers contention, query latency under background catch-up.
8. **Worst-case bench** — edits to high-degree hub notes, edits during cloud-sync burst, recovery after crash.

Use `criterion` for micro-bench and a custom harness for end-to-end. Land these as a `nestweaver-bench` crate or `benches/` directory before the brain extension reaches v1.

### B.5 Honesty addendum to be propagated into the main doc

When time permits, the main body should be updated with two edits:

1. **§8 Performance Design** — add a "Subject to validation" callout at the top of §8.1: every number here is a target informed by algorithmic reasoning, not a measured guarantee. Final numbers will be published with the v1 release after the bench suite (Appendix B.4) runs.
2. **§8.2 Where the time goes** — the per-stage timings listed there should be marked as estimates and pinned to specific assumptions (NVMe SSD, ~50K nodes, default embedding dim).

The numbers don't have to be perfect to ship. They have to be **honest**. A user who experiences a 200 ms query when the docs promised 100 ms loses trust faster than one who experiences 100 ms when the docs promised 250 ms.

---

---

## Appendix C — Benchmark Suite: Measured Numbers

The `benches/brain_benchmarks.rs` criterion suite measures the workloads the architecture body makes claims about. Run it via `cargo bench`. Notes count is configurable via `BENCH_NOTES` env var (default 1000) so larger scales don't require a recompile:

```sh
cargo bench                          # default 1K-note vault
BENCH_NOTES=5000 cargo bench         # 5K scale
BENCH_NOTES=50000 cargo bench        # 50K scale (per Appendix B.4)
```

### C.1 What's measured

| Bench | What it covers |
|---|---|
| `cold_index/notes=N` | Synthetic vault generation + full markdown index from scratch. Includes lbug DDL setup, comrak parse, batch insert, wikilink + tag resolution. |
| `brain_context_query/notes=N/seeds=3` | End-to-end `build_brain_context_hybrid` with 3 seeds, pure-PPR path (no Tantivy fusion). |
| `tantivy_search/notes=N` | BM25 query latency after the index is built and warm. |
| `ppr_compute/notes=N/scope=unified/iters=20` | Single `compute_pagerank` call over the unified scope, 20 iterations. |

What we deliberately *don't* benchmark in criterion:
- File-watcher event latency — depends on platform fs-event timing, which can't be measured deterministically. Covered by the `#[ignore]`d integration tests in `watcher::tests::*`.
- MCP tool round-trip — stdio framing dominates and adds noise. Covered by unit tests in `nestweaver-mcp::tests::*`.

### C.2 Indicative numbers (development run)

The numbers below were captured during initial development on a single workstation using `BENCH_NOTES=100 cargo bench -- --quick` (criterion `--quick` runs one iteration per benchmark — measurements are indicative, not statistically robust). They confirm the benchmark harness works and produce numbers in the expected order of magnitude. **For numbers fit to quote to users, run the full suite (no `--quick`) at the scales in Appendix B.4.**

| Workload | 100-note vault | Comment |
|---|---|---|
| `cold_index` | 15.5 s | Includes synthetic-vault file generation + lbug DDL setup (≈2–3 s amortised setup cost). Per-note indexing dominates the rest. |
| `brain_context_query` (3 seeds) | 36.8 ms | Below the architecture body's <100 ms p95 target. Tantivy fusion would add the BM25 search cost (≈60 µs measured separately). |
| `tantivy_search` | 58.5 µs | Two orders of magnitude faster than the body's <50 ms estimate — BM25 is essentially free at this scale. |
| `ppr_compute` (unified, 20 iters) | 5.57 ms | The body estimated 200 ms–1 s for 50K nodes; 100-note projection is consistent. |

### C.3 What needs to be benchmarked at scale before shipping numbers to users

Per Appendix B.4's pre-ship requirements, the following still need full criterion runs at 5K and 50K:

- [ ] `cold_index` at 5K and 50K — validate the architecture body's "<5 s for 1K / <2 min for 50K" claim (the body was revised once already after the discovery of optimistic timing).
- [ ] `brain_context_query` at 50K — validate the <100 ms p95 target with cold and hot PPR vectors.
- [ ] `ppr_compute` at 50K — validate the 200 ms–1 s range.
- [ ] `tantivy_search` at 50K — extrapolate scaling; expected near-linear with corpus size.

Pull the numbers into this appendix once captured. Until then, the body's performance claims remain at the SUSPECT/UNPROVEN tiers documented in Appendix B.2.

### C.4 Running the suite

```sh
# Default — 1K notes, full criterion (samples=100, 5 iters/sample)
cargo bench

# Quick check — 100 notes, one iteration per bench
BENCH_NOTES=100 cargo bench -- --quick

# Scale-out
BENCH_NOTES=5000 cargo bench -- --warm-up-time 1 --measurement-time 10
BENCH_NOTES=50000 cargo bench -- --warm-up-time 2 --measurement-time 30

# Single bench only
cargo bench --bench brain_benchmarks brain_context_query

# HTML report (criterion default location)
open target/criterion/report/index.html
```

Bench output goes to `target/criterion/` as both per-bench JSON and a top-level HTML report. CI integration is a v2 concern — for now, treat these numbers as developer-facing.

---

*End of design document. Companion document: `docs/plans/markdown-brain-extension.md` for effort estimation and recommended phasing.*
