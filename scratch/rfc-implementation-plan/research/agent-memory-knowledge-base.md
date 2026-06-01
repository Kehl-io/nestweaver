# Research Foundation: Agent Memory & Knowledge-Base RFC Features

Research grounding for three NestWeaver RFC features. All cited sources were retrieved via web search/fetch in May 2026. Sources without a retrieved URL are marked `[UNVERIFIED]`. Quantitative formulas/numbers are quoted from the primary source where possible.

NestWeaver context: Rust code-and-notes graph intelligence tool. It ingests an Obsidian-style markdown vault ("brain") with wikilinks, tags, frontmatter, notes/headings/sections, and a 4-tier knowledge pipeline: `_logs` (working) → `_ideas` (episodes) → `Projects/*/sync.md` (knowledge) → `_brain/conventions/*` (procedures). Embedded graph DB (LadybugDB); MCP server. Solo-dev.

---

## Cross-cutting source library (with exact results that matter)

### Agent / LLM memory architectures

**Park, Joon Sung; O'Brien, Joseph C.; Cai, Carrie J.; Morris, Meredith Ringel; Liang, Percy; Bernstein, Michael S. (2023). "Generative Agents: Interactive Simulacra of Human Behavior." UIST '23 (ACM).**
- arXiv: https://arxiv.org/abs/2304.03442 ; full HTML retrieved: https://ar5iv.labs.arxiv.org/html/2304.03442 ; ACM: https://dl.acm.org/doi/fullHtml/10.1145/3586183.3606763
- **The result that matters (exact, quoted from the paper):**
  - Retrieval score is a weighted sum of three components, each min-max normalized to [0,1]: `score = α_recency · recency + α_importance · importance + α_relevance · relevance`. "In our implementation, all αs are set to 1."
  - **Recency** = "exponential decay function over the number of sandbox game hours since the memory was last retrieved. Our decay factor is 0.995." (per game-hour)
  - **Importance** = LLM-rated 1–10 ("On the scale of 1 to 10, where 1 is purely mundane … and 10 is extremely poignant … rate the likely poignancy"). Computed once at memory creation.
  - **Relevance** = cosine similarity between the memory's embedding and the query's embedding.
  - **Reflection** triggers "when the sum of the importance scores for the latest events … exceeds a threshold (150 in our implementation)" → ~2–3 reflections/day. Reflections are higher-level memories synthesized from recent memories and stored back in the stream (recursive).
- Why it matters here: this is the canonical, citable recency/importance/relevance retrieval scoring. Directly maps to Feature 1's decay + ranking.

**Packer, Charles; Wooders, Sarah; Lin, Kevin; Fang, Vivian; Patil, Shishir G.; Gonzalez, Joseph E. (2023). "MemGPT: Towards LLMs as Operating Systems." arXiv:2310.08560 (UC Berkeley). Now the Letta framework.**
- https://arxiv.org/abs/2310.08560
- The result that matters: OS-inspired **tiered/virtual memory** — a small in-context "main memory" + large out-of-context storage, with the agent paging data between tiers via function calls. Maps to NestWeaver's tiered sidecar idea and the 4-tier vault promotion (main context = hot working set; archival = cold tiers).

**Zhong, Wanjun; Guo, Lianghong; Gao, Qiqi; Ye, He; Wang, Yanlin (2024). "MemoryBank: Enhancing Large Language Models with Long-Term Memory." AAAI 2024.**
- https://arxiv.org/abs/2305.10250 ; HTML: https://ar5iv.labs.arxiv.org/html/2305.10250
- **The result that matters (exact formula, quoted):** memory retention follows the Ebbinghaus forgetting curve `R = e^(-t/S)`, where `R` = "what fraction of the information can be retained", `t` = "time elapsed since learning", `S` = "memory strength". Implementation: `S` initialized to 1; on recall, "S is increased by 1 and t is reset to 0, hence forget it with a lower probability." Authors note this is "an exploratory and highly simplified memory updating model."
- Why it matters: gives a citable, recall-reinforced exponential-decay model where repeated access strengthens a memory (directly applicable to Feature 1's Access/Followup reinforcement and Feature 11's consolidation-by-repetition).

**Xu, Wujiang; Liang, Zujie; Mei, Kai; Gao, Hang; Tan, Juntao; Zhang, Yongfeng (2025). "A-MEM: Agentic Memory for LLM Agents." NeurIPS 2025.**
- https://arxiv.org/abs/2502.12110 ; HTML: https://arxiv.org/html/2502.12110v11 ; code: https://github.com/WujiangXu/A-mem
- **The result that matters (exact, quoted):** Each memory note `m_i = {c_i, t_i, K_i, G_i, X_i, e_i, L_i}` = content, timestamp, LLM-generated **keywords** `K`, LLM-generated **tags** `G`, LLM-generated **contextual description** `X`, embedding `e`, and **linked memories** `L`. Linking: cosine similarity `s_{n,j}` over note embeddings, retrieve **top-k nearest neighbors** (default k=10, task-tuned 10–50), then "LLM to analyze potential connections based on their potential common attributes." **Memory evolution:** adding a new note can rewrite existing neighbors' context/tags (`m_j* ← LLM(...)`). Explicitly **Zettelkasten-inspired** ("atomic note-taking and flexible organization"; notes can belong to multiple conceptual "boxes").
- Why it matters: A-MEM is the strongest prior art for typed/derived links over notes (Feature 11) and for Zettelkasten-style atomic notes in an agent-memory context. The "memory evolution" rewrite is a cautionary pattern (mutation of prior notes = provenance/audit concerns for a solo dev's brain).

**Zhang, Zeyu; Bo, Xiaohe; Ma, Chen; Li, Rui; Chen, Xu; Dai, Quanyu; Zhu, Jieming; Dong, Zhenhua; Wen, Ji-Rong (2024). "A Survey on the Memory Mechanism of Large Language Model based Agents." arXiv:2404.13501; accepted ACM TOIS July 2025.**
- https://arxiv.org/abs/2404.13501 ; resources: https://github.com/nuster1128/LLM_Agent_Memory_Survey
- Use as the umbrella survey citing memory sources/forms, read/write/reflect operations, and evaluation. Good for taxonomy framing and "established vs speculative" claims.

**[UNVERIFIED] Ebbinghaus, Hermann (1885). "Über das Gedächtnis" (Memory: A Contribution to Experimental Psychology).** Original forgetting-curve work; cite via MemoryBank's usage rather than a retrieved primary URL.

### Personal knowledge management (PKM)

**Ahrens, Sönke (2017). "How to Take Smart Notes."** — Modern systematization of Niklas Luhmann's Zettelkasten. Three note types: **fleeting** (raw captures), **literature** (notes on sources), **permanent** (atomic, linked, in your own words; "never thrown away"). Luhmann's slip-box: ~90,000 cards → 70 books + 400+ papers.
- Retrieved overview/explainers: https://zettelkasten.de/posts/concepts-sohnke-ahrens-explained/ ; https://www.ernestchiang.com/en/posts/2025/sonke-ahrens-how-to-take-smart-notes/
- Maps to NestWeaver's tier model: `_logs` ≈ fleeting, `_ideas` ≈ permanent/atomic, conventions ≈ distilled principles.

**Matuschak, Andy. "Evergreen notes."** — Notes should be **atomic** (single idea), **concept-oriented** (factored by concept, not source/project), and **densely linked**. "Notes … written and organized to evolve, contribute, and accumulate over time, across projects."
- https://notes.andymatuschak.org/Evergreen_notes ; https://notes.andymatuschak.org/Evergreen_notes_should_be_concept-oriented ; https://notes.andymatuschak.org/Evergreen_notes_should_be_atomic
- Maps to promotion criteria (Feature 11): a log section becomes an "idea" when it stabilizes into a concept-oriented, densely-linked unit.

**Forte, Tiago. "The PARA Method" / "Building a Second Brain."** — Organize by **actionability** not topic: **P**rojects (deadlined), **A**reas (ongoing), **R**esources (future-useful), **A**rchives (inactive). "Organizing by topic is the wrong approach."
- https://fortelabs.com/blog/para/ ; https://www.buildingasecondbrain.com/para
- Maps to: Archives ≈ a destination for superseded/stale notes; the actionability axis informs tier semantics (`Projects/*/sync.md` = active knowledge).

### Memory-bank pattern in AI coding tools (prior art for Feature 11)

**Cline "Memory Bank"** — structured docs the agent reads at session start: `projectbrief.md`, `productContext.md`, `activeContext.md`, `systemPatterns.md`, `techContext.md`, `progress.md`. Cycle: read → verify → execute → update. Goal: persist context across sessions, avoid context bloat.
- https://docs.cline.bot/prompting/cline-memory-bank ; https://cline.bot/blog/memory-bank-how-to-make-cline-an-ai-agent-that-never-forgets

**Roo Code Memory Bank** — `memory-bank/` with `activeContext.md`, `productContext.md`, `progress.md`, `decisionLog.md`, `systemPatterns.md`, `projectBrief.md`; integrated into VS Code modes that update specific files.
- https://github.com/GreatScottyMac/roo-code-memory-bank
- Tradeoff vs NestWeaver: these are **flat markdown conventions with no graph, no health checks, no typed edges, no decay**. NestWeaver's value-add is exactly the graph + maintenance layer on top of this pattern.

### Knowledge-graph quality, maintenance, link prediction

**Paulheim, Heiko (2017). "Knowledge graph refinement: A survey of approaches and evaluation methods." Semantic Web Journal 8(3).**
- https://www.semantic-web-journal.net/system/files/swj1167.pdf ; https://dl.acm.org/doi/10.3233/SW-160218
- The result that matters: refinement = **completion** (add missing knowledge / increase coverage) + **error detection** (find wrong statements / increase correctness). Error-detection methods "usually output a list of potentially erroneous statements," and notes that deriving **higher-level patterns** from errors (design-level problems) is rare but valuable. → directly frames Feature 9/11 health checks as "error detection" and link prediction as "completion."

**Liben-Nowell, David; Kleinberg, Jon (2003). "The Link Prediction Problem for Social Networks." CIKM '03.**
- https://www.cs.cornell.edu/home/kleinber/link-pred.pdf
- The result that matters: link prediction from **topology alone** is feasible on co-authorship graphs; "fairly subtle measures … can outperform more direct measures." **Adamic–Adar** performed best among the similarity indices tested; Jaccard worst (frequently replicated). → cite for "suggest-links" / typed-edge candidate generation over the wikilink graph.
- Heuristic family (overview, retrieved): Common Neighbors, **Adamic–Adar** (inverse-log-degree weighting of shared neighbors), Jaccard, Preferential Attachment, Katz; embedding methods DeepWalk/LINE/node2vec; GNNs for attributed graphs. https://en.wikipedia.org/wiki/Link_prediction

**Patrucco / "Orphan Articles: The Dark Matter of Wikipedia" (2023). arXiv:2306.03940.**
- https://arxiv.org/html/2306.03940v2
- **The result that matters (exact, quoted):** orphan = "articles without any incoming links from other Wikipedia articles" (same-language main namespace). **8.8M (14.7%)** of ~60M articles across 319 editions are orphans; ~**15%** "de facto invisible to readers navigating Wikipedia." English Wikipedia is an outlier at ~5% (still >300K). Orphans get far fewer pageviews ("mean for non-orphans is twice as high"); **de-orphanization → statistically significant +6.5% pageviews**, persistent, "mostly driven by readers using the newly added incoming links." FindLink (string-match) only covers 1.6M (18%); cross-lingual link translation covers 5.5M (62%).
- → cite for orphan detection value (Feature 9) AND the orphan-false-positive caveat: top-level index/MOC notes legitimately have no inbound links.

**Wikipedia operational practice:** WP:Orphan (orphan = no inbound mainspace links), WikiProject Orphanage (de-orphaning backlog ~80K in 2023, down from 140K peak in 2017), WP:Link rot / IABOT bot for dead external links.
- https://en.wikipedia.org/wiki/Wikipedia:Orphan ; https://en.wikipedia.org/wiki/Wikipedia:Link_rot

**Concept/schema drift:**
- "OntoDrift: a Semantic Drift Gauge for Ontology Evolution Monitoring." CEUR Vol-2821. https://ceur-ws.org/Vol-2821/paper1.pdf — detects/assesses semantic drift between time-distinct ontology versions via intension/extension/labels.
- "Do you catch my drift? On the usage of embedding methods to measure concept shift in knowledge graphs." (2023). https://dl.acm.org/doi/fullHtml/10.1145/3587259.3627555 — embedding-based drift measurement.
- → cite for Feature 11's "schema drift" health check (frontmatter key/value distributions changing over time).

### Community detection

**Traag, V. A.; Waltman, L.; van Eck, N. J. (2019). "From Louvain to Leiden: guaranteeing well-connected communities." Scientific Reports 9, 5233.**
- https://www.nature.com/articles/s41598-019-41695-z ; arXiv: https://arxiv.org/abs/1810.08473
- **The result that matters (quoted/paraphrased):** Louvain can yield "arbitrarily badly connected" or even **disconnected** communities — empirically up to 25% badly connected, up to 16% disconnected when run iteratively. **Leiden guarantees connected communities**, converges to a partition where all subsets are locally optimally assigned, runs faster, and finds better partitions. → cite as the reason to use Leiden (not Louvain) for topic clustering over the wikilink subgraph (Feature 9/11).

### Semantic-relation standards (for typed edges, Feature 11)

**W3C SKOS — Simple Knowledge Organization System Reference (2009).** https://www.w3.org/TR/skos-reference/
- `skos:broader` / `skos:narrower` = hierarchical; `skos:related` = associative (non-hierarchical). → maps `DependsOn`-ish hierarchy and `RelatesTo`.

**W3C PROV-O — The PROV Ontology (2013).** https://www.w3.org/TR/prov-o/
- `prov:wasDerivedFrom` = "a transformation of one entity into another" (derivation chain between entities); `prov:wasInformedBy` = dependency between activities; `prov:wasRevisionOf` (a PROV-O subtype of derivation) = entity is a revised version of another. → maps `Supersedes`/`CausedBy`/derivation. NestWeaver's `Supersedes` ≈ inverse of `prov:wasRevisionOf`; `CausedBy` ≈ inverse of `prov:wasDerivedFrom`/`wasInformedBy`.
- schema.org also has informal equivalents (`schema:isBasedOn`, `schema:supersededBy` [deprecated in schema.org core but conceptually present]) — prefer SKOS/PROV-O as the citable standards.

---

## FEATURE 1 — Interaction-memory event sidecar (Query / Access / Impact / Followup / TerminalSuccess), time-decay, pruning

### 1. Research foundation
- **Generative Agents** (Park et al. 2023, UIST; https://ar5iv.labs.arxiv.org/html/2304.03442): the memory-stream object + `score = recency + importance + relevance` (all α=1, min-max normalized) and **recency = exponential decay, factor 0.995 per game-hour since last retrieval**. This is the load-bearing precedent for event-based memory with decay scoring.
- **MemoryBank** (Zhong et al. 2024, AAAI; https://ar5iv.labs.arxiv.org/html/2305.10250): **`R = e^(-t/S)`** with strength `S` incremented and `t` reset on recall. This gives the *reinforcement-on-access* semantics that NestWeaver's `Access`/`Followup`/`TerminalSuccess` events need.
- **MemGPT** (Packer et al. 2023; https://arxiv.org/abs/2310.08560): tiered memory + paging — supports a hot/cold split for the sidecar and bounded in-context working set.
- **Survey** (Zhang et al. 2024, TOIS; https://arxiv.org/abs/2404.13501): taxonomy of read/write/reflect + evaluation for grounding design choices.

### 2. Prior art / projects + tradeoffs
- Generative Agents reference implementation (open source) — full memory-stream + reflection; heavyweight (LLM call per importance score, per reflection). Tradeoff: LLM-scored importance is expensive; NestWeaver can substitute **structural importance** (PageRank of the accessed symbol/node) for a zero-LLM proxy.
- MemoryBank/SiliconFriend (https://github.com/zhongwanjun/MemoryBank-SiliconFriend) — simplest citable decay+reinforce model; authors admit it's "highly simplified" (so don't over-engineer).
- NestWeaver already ships `<db>.interactions.json` and `nestweaver interactions status/clear` + `mcp --track-interactions` (per CLAUDE.md), so this feature formalizes an existing sidecar.

### 3. Recommended approach for NestWeaver (grounded)
- **Event record:** `{ event_id, session_id, ts, kind, target_uid(s), query_text?, intent?, weight }`. `session_id` groups events for **Followup** detection (an Access shortly after a Query in the same session) and for **TerminalSuccess** attribution (which targets were in-context when the session ended successfully).
- **Event kinds → weight semantics** (analogous to MemoryBank strength increments, and to the per-edge-type PPR weights NestWeaver already uses):
  - `Query` — seeds a session; low standalone weight.
  - `Access` — a symbol/note was actually opened/returned; primary reinforcement (`S += 1`, reset `t`).
  - `Impact` — appeared in an impact/blast-radius result; medium weight (signal of structural relevance).
  - `Followup` — accessed *after* a prior query in same session within a window; strong relevance signal (the first result wasn't enough → this one was the real target). Boost the followed-up target.
  - `TerminalSuccess` — session ended in a success signal (commit, test pass, user-confirmed). Retroactively boost all targets accessed in that session. This is the highest-value, sparsest signal — treat like Generative Agents "importance."
- **Decay (cite MemoryBank exactly):** per-target retention `R = exp(-Δt / S)`, where `Δt` = time since last event on that target, `S` = strength = accumulated event weight (each Access/Followup/TerminalSuccess raises `S`, resetting `Δt`). Use this `R` as a **recency-weighted boost on ranking** (multiply into search/PPR scores), mirroring Generative Agents' recency term.
- **Combined retrieval boost (cite Generative Agents):** `boost = w_r·R + w_i·importance + w_rel·relevance` where `importance` = node PageRank (structural proxy for LLM poignancy) and `relevance` = query–target embedding cosine. Start with equal weights (Generative Agents used α=1 each) and tune.
- **Pruning:** drop events whose contribution `R` falls below ε (e.g., `R < 0.01`) — equivalent to a hard age cutoff that lengthens with strength `S`, so frequently-useful targets persist (exactly the Ebbinghaus reinforcement intent). Cap total events per target (keep newest N + the TerminalSuccess events) to bound the sidecar. This is a deliberately bounded, append-mostly JSON log compacted on `index`/`watch`.
- **No-LLM by default:** importance via PageRank, relevance via existing embeddings — keeps the sidecar cheap and offline, unlike Generative Agents' per-memory LLM calls.

### 4. Pitfalls / failure modes + mitigations
- **Memory bloat / unbounded JSON.** Append-only event logs grow without compaction. → Compact on each `index`/`watch` cycle; prune by `R < ε`; cap per-target event count.
- **Popularity bias / rich-get-richer.** Frequently-accessed hubs accumulate strength and dominate ranking, drowning new/cold nodes (same pathology as PageRank + reinforcement). → Cap the max boost; normalize boost per query (min-max like Generative Agents); decay strength `S` slowly even on hubs; exclude very-high-degree hubs from boost.
- **TerminalSuccess attribution error.** A session touches many targets; not all caused the success. → Weight by recency-within-session and by whether the target was a `Followup` (deliberate) vs incidental Access; require multiple successes before strong boost.
- **Privacy / query-text leakage in the sidecar.** Query strings persisted to disk. → Store hashes/embeddings, not raw text, by default; `interactions clear` already exists; make tracking opt-in (it already is per CLAUDE.md).
- **Clock/`Δt` correctness across re-index.** Decay depends on wall-clock; watcher restarts mustn't reset `t`. → Persist last-event ts per target; compute `Δt` from persisted ts.

### 5. Complexity / effort
- **Low–Medium.** Sidecar + decay scoring is small (the JSON sidecar and CLI already exist). The work is: (a) defining the event schema + session grouping, (b) the `R = e^{-Δt/S}` decay + ranking integration into existing search/PPR, (c) compaction/pruning on index. No new heavy deps. Estimate: a few days; the ranking-integration tuning is the open-ended part.

---

## FEATURE 11 — Memory-bank semantics over the vault: typed edges, 7 health checks, consolidation pipeline

### 1. Research foundation
- **Typed/derived links from notes:** A-MEM (Xu et al. 2025, NeurIPS; https://arxiv.org/abs/2502.12110) — LLM-derived keywords/tags/context + top-k embedding linking + "memory evolution"; explicitly Zettelkasten.
- **Standards for the relation types:** SKOS (https://www.w3.org/TR/skos-reference/) and PROV-O (https://www.w3.org/TR/prov-o/).
- **Health checks as KG refinement:** Paulheim 2017 (https://www.semantic-web-journal.net/system/files/swj1167.pdf) — error detection vs completion framing; the 7 checks are mostly **error detection**.
- **Orphans/broken links:** "Orphan Articles" (arXiv:2306.03940) + WP:Orphan/WP:Link rot.
- **Schema drift:** OntoDrift (https://ceur-ws.org/Vol-2821/paper1.pdf) + embedding-drift (https://dl.acm.org/doi/fullHtml/10.1145/3587259.3627555).
- **Consolidation/promotion through tiers:** PKM tier models — Ahrens fleeting→literature→permanent; Matuschak evergreen (atomic, concept-oriented, densely-linked); MemoryBank reinforcement-on-recall (`R=e^{-t/S}`); Generative Agents reflection-threshold (importance sum > 150 → synthesize higher-level memory). These justify "linked from 3+ notes AND survived 14 days → promote."

### 2. Prior art / projects + tradeoffs
- **Cline / Roo Memory Bank** (https://docs.cline.bot/prompting/cline-memory-bank , https://github.com/GreatScottyMac/roo-code-memory-bank): the namesake pattern. Flat markdown, agent-maintained, **no graph / no typed edges / no health checks / no decay**. NestWeaver differentiates by adding the graph + maintenance + promotion layer. Tradeoff: more machinery; mitigate by keeping checks read-only/advisory.
- **A-MEM:** strongest research analog. Cautionary: its "memory evolution" **mutates prior notes**, which for a solo dev's source-controlled brain is risky (provenance loss). → NestWeaver should *suggest* edges/promotions and record provenance (PROV-O style), not silently rewrite notes.
- **Obsidian/Foam/Dataview ecosystem** [UNVERIFIED specifics]: provide backlinks, unlinked-mentions, orphan/broken-link panels — but no typed-relation derivation or tier promotion. NestWeaver's typed edges + consolidation are the novel part.

### 3. Recommended approach for NestWeaver (grounded)
**Typed edges (derive from frontmatter keys + section names; map to standards):**
| NestWeaver edge | Standard mapping | Derivation source |
|---|---|---|
| `Supersedes` | inverse of `prov:wasRevisionOf` (a PROV-O derivation subtype) | frontmatter `supersedes:`/`superseded_by:`; section "Replaces"; status: deprecated + link |
| `DependsOn` | `skos:broader` (hierarchical) / `prov:wasInformedBy` | frontmatter `depends_on:`/`requires:`; section "Prerequisites"/"Depends on" |
| `CausedBy` | `prov:wasDerivedFrom` / `prov:wasInformedBy` | frontmatter `caused_by:`/`because:`; section "Caused by"/"Root cause" |
| `RelatesTo` | `skos:related` (associative) | plain wikilinks not matching a typed pattern; section "Related"/"See also" |
- Derivation = deterministic rules over frontmatter keys and heading text (no LLM needed for v1; A-MEM shows LLM derivation is possible but adds cost + mutation risk). Keep raw wikilinks as `RelatesTo` and *upgrade* to typed when a pattern matches.

**7 health checks (frame as Paulheim error-detection; one is completion):**
1. **Stale** — note not modified in N days AND/OR retention `R=e^{-t/S}` below threshold (cite MemoryBank). Solo-dev tuning, e.g. 90 days.
2. **Contradictions** — two notes both claim authority on a topic with no `Supersedes` between them (e.g., two conventions for the same procedure). Detect via shared tags/title + status conflict.
3. **Orphans** — no inbound typed/wiki links (cite WP:Orphan + arXiv:2306.03940). **Exclude index/MOC notes** (see pitfalls).
4. **Broken wikilinks** — link target file/heading does not exist (cite WP:Link rot).
5. **Supersession chains** — follow `Supersedes` chains; flag (a) cycles, (b) chains where a superseded note is still linked as if current, (c) >K-deep chains needing cleanup.
6. **Schema drift** — frontmatter key set / value vocab for a note class diverges over time (cite OntoDrift). E.g., `status` values fragmenting into `done/complete/finished`.
7. **Dangling relationships** — a typed edge whose target is missing or wrong-typed (e.g., `depends_on:` pointing to a deleted note). PROV-O/SKOS integrity.

**Consolidation / promotion pipeline (4-tier):** `_logs → _ideas → Projects/*/sync.md → _brain/conventions/*`.
- Promotion is **threshold-gated**, grounded in three citable mechanisms: (a) **reinforcement** (MemoryBank: survive/strengthen with repeated reference), (b) **reflection threshold** (Generative Agents: accumulate enough "importance" then synthesize a higher-level unit — here, importance ≈ number of inbound links × access events), (c) **evergreen criteria** (Matuschak: atomic + concept-oriented + densely-linked = ready to be permanent).
- Concrete rule (the RFC's example, now grounded): a `_logs` section that is **linked from ≥3 notes** (densely-linked, Matuschak) **and has survived ≥14 days** (stability/reinforcement, MemoryBank/Ebbinghaus) → promote to `_ideas`. Analogous gates for ideas→sync and sync→conventions (e.g., referenced across ≥2 projects, stable ≥30 days, low contradiction).
- **Promotion = suggest + record provenance**, not silent move. Emit a `prov:wasDerivedFrom` edge from the promoted idea back to the source log (audit trail; avoids A-MEM mutation risk).

### 4. Pitfalls / failure modes + mitigations
- **Orphan false positives for index/MOC notes.** Top-level maps-of-content and home notes legitimately have zero inbound links (Matuschak MOCs; PARA top folders). → Exclude notes tagged `#index`/`#moc`, notes in known index paths, and notes with high *outbound* degree (hubs are sources, not orphans).
- **False consolidation / premature promotion.** A log briefly hot (3 links in one burst) but never revisited gets promoted as evergreen → tier pollution. → Require *temporal spread* (links arrive over time, not one commit) + survival window + low contradiction; make promotion a *suggestion* the dev confirms.
- **Stale-check false positives for reference/convention notes.** Conventions are *meant* to be stable and rarely edited; flagging them stale is wrong. → Tier-aware staleness: conventions get a far longer/disabled staleness window.
- **Schema-drift over-flagging during legitimate evolution.** A solo dev refining their own frontmatter vocab will trip drift constantly. → Drift = advisory only; require a sustained distribution shift (not single edits); allow an alias map (NestWeaver already has `<db>.aliases.json`).
- **Typed-edge misclassification from heading text.** "See also: X" vs "Depends on: X" — regex misreads. → Conservative rules; default to `RelatesTo` when ambiguous; never auto-`Supersedes` (high-consequence) without explicit frontmatter.
- **Provenance loss if promotion mutates source.** → Never delete the source log on promotion; keep `wasDerivedFrom` link; let archival follow PARA Archives semantics.

### 5. Complexity / effort
- **Medium–High.** Typed-edge derivation = small (rules over existing parsed frontmatter/sections). The 7 health checks = mostly graph traversals over the existing store (Low–Medium each; contradictions and schema-drift are the hardest). Consolidation pipeline is the heaviest: requires temporal tracking (which NestWeaver partly has via `graph_generation` counter + `<db>.filemeta.json` mtimes), promotion gating, suggestion surface, and provenance edges. Estimate: 1–2 weeks, with consolidation as the long pole and the highest false-positive risk.

---

## FEATURE 9 — First-class document-graph ops: broken-link, orphan, Leiden topic clustering, tag co-occurrence, doc health stats

### 1. Research foundation
- **Orphans:** "Orphan Articles: The Dark Matter of Wikipedia" (arXiv:2306.03940; https://arxiv.org/html/2306.03940v2) — orphan = no inbound links; **14.7%** of Wikipedia orphaned; **de-orphaning → +6.5% pageviews**. Quantifies *why* orphan detection matters. WP:Orphan operational definition.
- **Broken/dead links:** WP:Link rot + IABOT (https://en.wikipedia.org/wiki/Wikipedia:Link_rot).
- **Topic clustering:** **Leiden** (Traag et al. 2019, Sci. Rep.; https://www.nature.com/articles/s41598-019-41695-z) — guarantees connected communities; Louvain produces up to 16% disconnected communities. Use Leiden over the wikilink subgraph.
- **Refinement framing:** Paulheim 2017 — these ops are completion + error detection on the doc graph.
- **Link suggestion (completion):** Liben-Nowell & Kleinberg 2003 (https://www.cs.cornell.edu/home/kleinber/link-pred.pdf) — Adamic–Adar best topological predictor; basis for "you should link these two notes."

### 2. Prior art / projects + tradeoffs
- **Obsidian core + Graph Analysis/Foam** [UNVERIFIED feature specifics]: backlinks, local graph, orphan & broken-link panels, some co-occurrence; clustering is usually force-layout visual, **not algorithmic community detection**. NestWeaver's edge = running **Leiden** + producing structured stats/MCP tools, not just a force graph.
- **Wikipedia tooling** (FindLink, IABOT): production-grade orphan/dead-link tooling but Wikipedia-specific. Confirms the ops are real and valuable; FindLink's 18% coverage shows pure string-match is weak → use graph-topology link prediction (Adamic–Adar) instead.
- NestWeaver already computes communities (`nestweaver clusters`, `<db>.clusters.json`, adaptive resolution) over the *code* graph — Feature 9 reuses that machinery on the *wikilink* subgraph. Low marginal cost.

### 3. Recommended approach for NestWeaver (grounded)
- **Broken-link detection:** resolve every wikilink target (note + heading/anchor) against the indexed vault; report unresolved. Distinguish broken-internal (fixable) from external URL rot (out of scope or async-checked). Cite WP:Link rot.
- **Orphan detection:** zero inbound wiki/typed links, **excluding index/MOC notes** (tag/path/outbound-degree heuristic). Report with a "would-de-orphan" suggestion (top Adamic–Adar candidates) — directly mirrors the arXiv:2306.03940 finding that adding inbound links drives discoverability (+6.5%).
- **Topic clustering — use Leiden, not Louvain** (cite Traag et al. 2019): run on the undirected wikilink subgraph (optionally weighted by co-link count). NestWeaver already has adaptive-resolution clustering; ensure the algorithm is Leiden (connected-community guarantee) for the sparser, noisier wikilink graph. Output named clusters (label by top tags/title terms per cluster).
- **Tag co-occurrence graph:** nodes = tags, edge weight = number of notes sharing both tags; surfaces de-facto taxonomy and synonym candidates (feeds schema-drift/alias detection in Feature 11). Can also run Leiden on it to find tag communities.
- **Doc health stats:** counts of orphans, broken links, %-orphaned (compare to Wikipedia's ~15% as a reference point), avg/median link degree, largest connected component size, cluster count/modularity, stale-note count. Expose as a `doc-health` command + MCP tool (promotes these from internal to first-class, per the feature title).

### 4. Pitfalls / failure modes + mitigations
- **Orphan false positives (index/MOC/entry notes).** Same as Feature 11 — biggest accuracy risk. → Exclude by tag/path/high-outbound-degree; report separately as "intentional hubs."
- **Leiden over a sparse/disconnected wikilink graph → many singletons.** Personal vaults are sparser than co-authorship nets; resolution matters. → Tune resolution (NestWeaver's adaptive resolution already does this for size), report singletons as orphan-cluster candidates, run on the largest connected component, optionally fold in tag co-occurrence edges to densify.
- **Tag co-occurrence dominated by a few mega-tags** (e.g., `#project` on everything). → Weight by PMI/normalized co-occurrence, not raw counts; cap or drop ubiquitous tags.
- **Broken-link false positives from aliases/path resolution.** Obsidian shortest-path links, aliases, heading slugging. → Reuse NestWeaver's existing resolver + `<db>.aliases.json`; match Obsidian's link-resolution rules exactly before flagging.
- **External-URL checking is slow/flaky.** → Keep internal vs external separate; make external dead-link checks opt-in/async (like IABOT runs continuously, not inline).
- **Clustering instability across runs** (Leiden is stochastic). → Fix random seed; report only stable clusters or version clusters via `graph_generation`.

### 5. Complexity / effort
- **Low–Medium.** Broken-link + orphan detection = straightforward traversals over the existing store (Low). Leiden clustering + tag co-occurrence largely **reuse existing code-graph clustering machinery** applied to the wikilink subgraph (Low–Medium — main work is subgraph construction, MOC exclusion, and labeling). Doc-health stats + the new command/MCP tool surface = Low. Estimate: a few days to ~1 week; mostly integration and the orphan-exclusion heuristics, not new algorithms.

---

## Established vs speculative (summary)
- **Established / directly citable:** Generative Agents retrieval formula (α=1; decay 0.995/hr; reflection threshold 150); MemoryBank `R=e^{-t/S}`; Leiden's connected-community guarantee + Louvain 16% disconnected; Adamic–Adar as best topological link predictor; Wikipedia orphan 14.7% / +6.5% de-orphaning; SKOS/PROV-O relation semantics; Paulheim refinement = completion + error detection.
- **Speculative / NestWeaver-specific heuristics needing validation:** the exact promotion gates ("≥3 inbound links + 14 days"); mapping interaction event-kinds to specific `S` increments; using PageRank as the importance proxy in place of LLM poignancy; MOC-exclusion heuristics for orphan detection. These are *grounded by analogy* to the cited work but are design choices to be tuned empirically, not results from the literature.
- **`[UNVERIFIED]`:** Ebbinghaus 1885 primary text (cited via MemoryBank); Obsidian/Foam/Dataview specific feature claims (ecosystem knowledge, not retrieved this session).
