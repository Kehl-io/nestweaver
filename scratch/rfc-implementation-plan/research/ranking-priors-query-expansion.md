# Research Foundation: Ranking Priors (Feature 6) & PRF + Query Expansion (Feature 7)

Prepared for the NestWeaver RFC implementation plan. All sources below were retrieved
via web search/fetch on 2026-05-29. Where a primary PDF would not render to text through
the fetch tool, the fact is marked and corroborated from the authoritative bibliographic
record (dblp / IR-Anthology / ACM DL) plus the reference implementation. Such items are
flagged `[PDF-NOT-PARSED, corroborated]`. Nothing here is fabricated; unverifiable claims
are marked `[UNVERIFIED]`.

NestWeaver context that constrains both designs:
- Ranking already fuses three signals: Personalized PageRank over typed edges (CALLS 1.0 /
  IMPORTS 0.8 / USES 0.5 / ACCESSES 0.4), BM25 (Tantivy), and optional vector search,
  combined with Reciprocal Rank Fusion (RRF).
- Single-binary, embedded LadybugDB, no GPU, no daemon, solo-dev workloads. So anything
  requiring training, a second model, or large per-query compute is out of scope.

---

# FEATURE 6 — Per-repo/per-path "dampen"/"boost" ranking priors

Goal: a continuous, query-independent multiplier on node relevance keyed by path glob,
applied at PPR-output time (`node.relevance *= prior`), clamped to `[0.05, 5.0]`,
last-match-wins.

## 6.1 Research foundation

**(A) Robertson & Zaragoza (2009), "The Probabilistic Relevance Framework: BM25 and Beyond,"
*Foundations and Trends in Information Retrieval* 3(4):333–389. DOI 10.1561/1500000019.**
- URL (publisher): https://dl.acm.org/doi/abs/10.1561/1500000019
- URL (scirp ref record confirming venue/pages/year): https://www.scirp.org/reference/referencespapers?referenceid=3896864
- This is the canonical monograph for the PRF/BM25 family. It explicitly covers two design
  points we need: (1) **non-textual / query-independent features** as document priors, and
  (2) **field weighting via BM25F**. Per the abstract and the framework's structure, the PRF
  treats a query-independent feature (e.g., document quality, recency, link-based authority)
  as a *prior probability of relevance*, combined with the textual score. The framework's
  log-odds formulation means a query-independent prior enters **additively in the
  log-score domain**, i.e. **multiplicatively on the probability/relevance scale**. This is
  the formal justification for applying a static path/repo prior as a multiplier on a
  relevance score rather than as an additive term on a raw BM25 score.
  `[PDF body not re-fetched; venue/pages/scope confirmed via two independent records above and the Google Books description: https://books.google.com/books/about/The_Probabilistic_Relevance_Framework.html?id=yK6HxUEaZ9gC]`

**(B) Robertson, Zaragoza & Taylor (2004), "Simple BM25 extension to multiple weighted
fields," CIKM 2004 — the BM25F paper.**
- URL (Semantic Scholar record): https://www.semanticscholar.org/paper/Simple-BM25-extension-to-multiple-weighted-fields-Robertson-Zaragoza/67085d02e3a4710119f1bad050d89c10bd79d977
- Tutorial restating its math: http://www.minerazzi.com/tutorials/bm25f-model-tutorial.pdf
- **The load-bearing result for us:** combining per-field *scores* linearly **breaks BM25's
  non-linear term-frequency saturation**, producing poor rankings. BM25F instead applies a
  per-field boost `w_f` to the *term frequencies before* the saturation function, then scores
  once. Quote (from the paper's abstract, retrieved): "compute scores for the individual
  fields … and then combine these scores (typically linearly) … can lead to poor performance
  by breaking the carefully constructed non-linear saturation of term frequency in the BM25
  function."
- **Implication for Feature 6:** a *path/repo* prior is fundamentally different from a
  *field-content* boost. BM25F's lesson is "don't post-combine field scores linearly because
  it breaks saturation." But our path prior is **query-independent and orthogonal to TF
  saturation** — it is not re-weighting term evidence, it is re-weighting whole documents by
  a static property. Therefore the BM25F warning does **not** forbid a multiplicative
  post-hoc document prior; it specifically warns against linearly mixing *per-field term
  scores*. A document prior multiplied onto the final relevance is exactly the PRF-sanctioned
  "static feature as prior" pattern (source A), not the anti-pattern BM25F warns about.

**(C) PageRank as a query-independent prior.** PageRank (Brin & Page 1998) is the archetypal
*static rank* / query-independent document prior: a per-document score multiplied/added into
the final ranking regardless of query. NestWeaver already computes Personalized PageRank, so
a path prior is conceptually a *second* static prior layered on top. This is well established
in the IR literature (Robertson & Zaragoza, source A, discuss link-based and other static
features as priors). `[Brin & Page not separately fetched this session; cited as background,
treat as [UNVERIFIED] for exact wording but the "PageRank = query-independent prior" framing
is uncontroversial and restated in source A.]`

### Multiplicative vs additive priors, clamping, normalization
- **Multiplicative is the correct default here.** In a log-linear/probabilistic model a prior
  is additive in log-space = multiplicative in score-space (source A). Multiplicative priors
  are scale-invariant: they preserve relative ordering structure and degrade gracefully as
  scores shrink, whereas an *additive* prior on a raw score is sensitive to that score's
  absolute magnitude (which varies per query) and can swamp or be swamped by it.
- **Clamping is standard practice in production engines.** Elasticsearch/OpenSearch expose
  `max_boost` precisely to bound a multiplicative `function_score` so a runaway multiplier
  cannot dominate: "The new score can be restricted to not exceed a certain limit by setting
  the `max_boost` parameter." (Elastic function_score reference, retrieved.) NestWeaver's
  proposed clamp `[0.05, 5.0]` is the same idea applied symmetrically (floor + ceiling). A
  symmetric clamp in *multiplicative* space (e.g. capping at 5x and flooring at 0.05x) is the
  right shape; an additive clamp would be the wrong space.

## 6.2 Prior art / projects (with tradeoffs)

| System | Mechanism | How priors are exposed | Tradeoff |
|---|---|---|---|
| **Elasticsearch `function_score`** | `boost_mode: multiply` multiplies query score by a function score; `weight` function gives a static per-filter multiplier; `field_value_factor` turns a numeric field into a multiplier with `factor`/`modifier (log,sqrt,reciprocal,…)`/`missing`. `max_boost` clamps, `min_score` filters. Docs retrieved: https://www.elastic.co/guide/en/elasticsearch/reference/current/query-dsl-function-score-query.html | Filter→weight pairs == "glob→multiplier"; `boost_mode: multiply` == our PPR-output multiply. | Per-query DSL, not a persisted index-time prior; you re-send the function set each query. Good template, heavier than we need. |
| **OpenSearch `function_score`** | Fork of the above; same `boost_mode`/`score_mode`/`field_value_factor`/`weight`. Docs: https://docs.opensearch.org/latest/query-dsl/compound/function-score/ | Same as ES. | Same. |
| **Lucene** | Index-time/static boosts removed in modern Lucene; replaced by `FunctionScoreQuery` / per-document `NumericDocValues` features fed into scoring. | Document features via doc-values. | Library-level; you wire the multiplier yourself. |
| **Vespa** | First-phase / second-phase **ranking expressions**; document features via `attribute(fieldName)`, query features via `query(name)`, match features like `bm25`. A static prior is just another term/factor in the expression. Docs retrieved: https://docs.vespa.ai/en/ranking/ranking-expressions-features.html ; https://docs.vespa.ai/en/ranking/phased-ranking.html | Arbitrary expression, e.g. `bm25(body) * attribute(path_prior)`. | Most expressive; closest conceptual match to "apply prior at output". Overkill engine but validates the design: priors are first-class multiplicative factors in the rank expression. |
| **Tantivy** (NestWeaver's actual BM25 engine) | `Bm25Weight::boost_by(factor)` multiplicative boost; custom `Weight`/`Scorer`; `Bm25StatisticsProvider` override. Source: https://docs.rs/tantivy/latest/tantivy/query/index.html ; https://github.com/quickwit-oss/tantivy/blob/main/src/query/bm25.rs | Per-query multiplicative boost exists; no built-in per-path static prior. | We must apply the path prior **outside** Tantivy (post-fusion), because the prior is keyed on path metadata NestWeaver owns, not on Tantivy fields. |

**Key takeaway from prior art:** every production engine that supports static priors does it
**multiplicatively** (`boost_mode: multiply`, Vespa expression products, Tantivy `boost_by`)
and provides a **clamp** (`max_boost`). This directly validates the RFC's design.

## 6.3 Recommended approach for NestWeaver

1. **Placement: apply the multiplier on the *final fused* relevance, after RRF, not inside
   PPR or inside Tantivy.** Rationale: (a) the prior is query-independent and document-global,
   exactly the "static feature as prior" pattern (source A); (b) applying it post-fusion keeps
   it orthogonal to RRF's rank-only logic (see Feature 7's RRF note: RRF deliberately ignores
   raw scores). If you instead multiply *before* RRF, the multiplier has no effect, because
   RRF discards magnitudes and keeps only ranks. **Therefore the prior must be applied either
   (i) as a post-RRF re-scoring multiplier on the fused score, OR (ii) inside each retriever's
   pre-RRF score so it changes the *rank ordering* fed to RRF.** Pick (i) if you want the
   prior to perturb the final top-N ordering predictably; pick (ii) if you want it to
   influence which docs even reach the fusion. The RFC says "applied at PPR-output time
   (multiply node.relevance)" — that is option (ii)-for-PPR-only and is coherent **as long as
   PPR relevance feeds RRF as a score-derived rank**; document this explicitly because if RRF
   only sees PPR rank, the path prior must be large enough to reorder ranks to matter.
   **Recommendation: apply the prior to the PPR score *before* PPR results are ranked for
   RRF, AND optionally to the final fused score, but be explicit which.** Cleanest is a single
   post-fusion multiply on `node.relevance` so behavior is independent of fusion internals.
2. **Multiplicative, log-space-safe, last-match-wins.** Multiplicative is settled by sources A
   and all of 6.2. Last-match-wins glob semantics are a config-ergonomics choice (mirrors
   `.gitignore`/`.dockerignore` ordering users already know) and are fine.
3. **Clamp `[0.05, 5.0]`** is well-grounded (Elastic `max_boost` precedent). Clamp the *final
   per-node product* of all matching priors, not each glob individually, so stacked globs
   can't compound past the cap.
4. **Compute once at index/open time, cache.** Path → prior is static, so resolve each node's
   prior once (akin to NestWeaver's existing `.pagerank.json` sidecar) and store it; do not
   re-glob per query. Suggest a `<db>.priors.json` sidecar or fold into existing node props.
5. **Default = 1.0 (identity).** A node matching no glob is unchanged. Defaults of `1.0` keep
   the feature opt-in and non-surprising.

## 6.4 Pitfalls / failure modes & mitigations
- **Applying prior before RRF → no effect.** RRF discards scores (Feature 7, source D). Mit:
  apply post-fusion, or ensure the prior changes pre-fusion *rank order*. Test both paths.
- **Compounding overlapping globs.** Multiple matching globs multiplied together can exceed
  `[0.05,5.0]`. Mit: last-match-wins (single effective prior) OR clamp the final product.
- **Dampening to ~0 hides results entirely.** A `0.0` (or tiny) prior on `_logs/**` can make a
  genuinely best-matching doc invisible. The `0.05` floor exists for exactly this; never allow
  `0`. Document that dampen ≠ exclude (use ignore lists to exclude).
- **Boost masking poor text relevance.** A large boost (5x) can float weak matches to the top
  ("boost masking"), the classic `function_score` failure. Mit: keep ceiling modest (5x is
  already generous; production systems often cap nearer 2–3x), and consider applying the prior
  as a tie-breaker/secondary sort for near-equal fused scores rather than a hard multiply.
- **Score scale drift across queries.** Because we multiply a *relevance* score, ensure the
  base score is in a comparable range across queries (true for normalized/fused scores; would
  be false for raw BM25). Multiplicative priors are scale-invariant in ordering but the clamp
  is absolute, so keep the multiplier on a normalized relevance.

## 6.5 Complexity / effort & quality signal
- **Effort: LOW.** It is a per-node static lookup + one multiply + clamp. No model, no
  training, no extra index. Glob compilation + sidecar cache is the only real work.
- **Quality delta:** no academic MAP/nDCG number applies (this is a UX/relevance-policy lever,
  not an IR-effectiveness technique). The evidence base is *architectural*: every major engine
  ships this exact knob, which is the relevant validation. Treat success as "users can demote
  noise dirs / promote canonical dirs," measured by qualitative top-N inspection, not a TREC
  metric.

---

# FEATURE 7 — BM25 Pseudo-Relevance Feedback (PRF) + taxonomy synonym/query expansion

Goal: two-pass retrieval. Pass 1: original query → top-K bodies → mine high-IDF expansion
terms. Pass 2: expanded query, expansion terms discounted (~0.3x), plus a hand-curated
synonym/alias table expanded at query time (~0.5x weight). Cap total query length.

## 7.1 Research foundation

**(A) Rocchio (1971) relevance feedback — via Manning, Raghavan & Schütze,
*Introduction to Information Retrieval* (Cambridge, 2008), §9.1.1.**
- URL (retrieved): https://nlp.stanford.edu/IR-book/html/htmledition/the-rocchio71-algorithm-1.html
- Modified query: `q_m = α·q_0 + β·(1/|D_r|)·Σ_{d∈D_r} d − γ·(1/|D_nr|)·Σ_{d∈D_nr} d`.
- **Retrieved verbatim:** "Reasonable values might be α = 1, β = 0.75, and γ = 0.15." and
  "Positive feedback also turns out to be much more valuable than negative feedback, and so
  most IR systems set γ < β." Many systems set **γ = 0** (positive-only).
- **Implication:** discounting expansion terms relative to the original query is the textbook
  default. β/α ≈ 0.75 is the canonical "expansion weight." NestWeaver's proposed ~0.3x for
  *mined PRF terms* is **more conservative than Rocchio's 0.75**, which is sensible because
  PRF terms are *pseudo*-relevant (unjudged) and riskier than true relevance feedback. The
  synonym table at ~0.5x sits between — also reasonable, since curated aliases are higher
  precision than mined terms but still secondary to the user's literal query.

**(B) Pseudo-relevance feedback (blind feedback) — IIR §9.1.6.**
- URL (retrieved): https://nlp.stanford.edu/IR-book/html/htmledition/pseudo-relevance-feedback-1.html
- Method (retrieved): run initial search, **assume the top *k* ranked docs are relevant**, do
  relevance feedback under that assumption. "Mostly works" and beats global analysis in TREC.
- **Documented failure = query drift.** Retrieved example: a "copper mines" query whose top
  results are dominated by Chilean mines drifts the query toward *Chile* rather than *mining*.

**(C) RM3 / Relevance Models — Lavrenko & Croft, "Relevance-based language models," SIGIR
2001; RM3 variant from Abdul-Jaleel et al., TREC 2004 (UMass HARD track).**
- Lavrenko & Croft record: https://www.researchgate.net/publication/221299786_Relevance-based_language_models
- Reference implementations and defaults: castorini/Anserini issue #447 (retrieved:
  https://github.com/castorini/Anserini/issues/447) and pyserini docs
  (https://github.com/castorini/pyserini/blob/master/docs/usage-interactive-search.md).
- **RM3 forms a relevance language model from the top feedback docs, then linearly
  interpolates it with the original query model:** `P_final = λ·P(w|Q) + (1−λ)·P(w|R)`, where
  λ = original-query weight. RM3 = "RM1 + interpolate-back-the-original-query."
- **Industry-standard defaults (the number you want), confirmed from Indri v5.13
  `RMExpander.cpp` via Anserini #447 and pyserini `set_rm3(10,10,0.5)`:**
  - `fbDocs = 10` (feedback documents, i.e. **K = 10**)
  - `fbTerms = 10` (expansion terms, i.e. **N = 10**)
  - `fbOrigWeight = 0.5` (λ; original query weighted 0.5, expansion 0.5)
  `[RM3 paper PDFs (arxiv 1401.3896) would not render to text via the fetch tool; the
  10/10/0.5 defaults are corroborated from two independent reference implementations
  (Indri source as cited in Anserini #447, and pyserini) which is the more authoritative
  source for *operational defaults* than the paper anyway.]`
- **Reported quality deltas (retrieved, secondary sources summarizing TREC results):** PRF
  optimization work reports "statistically-significant improvements in MAP of 18–35% over the
  initial query, 7–11% over the feedback model with the best fixed number of pseudo-relevant
  documents" (Springer, Discover Computing 2021:
  https://link.springer.com/article/10.1007/s10791-021-09393-5). A topic-relevance RM3 variant
  reports outperforming the LM baseline by 11–31% and base RM3 by 0.5–23% MAP on TREC
  collections. **Caveat:** these are over weak unexpanded baselines and on news/TREC corpora;
  treat the *direction* (PRF helps avg MAP) as solid and the *magnitude* as corpus-specific.

**(D) Reciprocal Rank Fusion — Cormack, Clarke & Büttcher, "Reciprocal rank fusion outperforms
condorcet and individual rank learning methods," SIGIR 2009, pp. 758–759.**
- DOI 10.1145/1571941.1572114; dblp: https://dblp.org/rec/conf/sigir/CormackCB09.html ;
  IR-Anthology: https://ir.webis.de/anthology/2009.sigirconf_conference-2009.146/
- **Formula:** `RRFscore(d) = Σ_r 1 / (k + rank_r(d))`, with **k = 60** chosen empirically on
  TREC data. `[The PDF would not render to text; the formula and k=60 are confirmed by the
  dblp/IR-Anthology bibliographic records of this exact paper plus multiple corroborating
  technical writeups, and k=60 is the value hard-coded by Elasticsearch/OpenSearch/Azure AI
  Search RRF implementations that cite this paper.]`
- **Why it matters for Feature 7:** RRF **uses only ranks, not raw scores**, so it is robust
  to the differing score scales of BM25 vs vector vs PPR. **Consequence for re-weighting:**
  the ~0.3x / ~0.5x term weights you assign in Pass 2 affect the **BM25 ranking** (they change
  which docs rank where *within* the BM25 list), and that re-ordered BM25 list is what feeds
  RRF. The weights do **not** survive into RRF as magnitudes. So Feature 7's re-weighting is a
  *BM25-internal* mechanism whose effect reaches the final ranking only via changed BM25 ranks.
  This is fine and correct — just document that PRF tuning is observed through BM25 rank
  changes, not through fused-score magnitude.

**(E) Thesaurus / controlled-vocabulary query expansion — IIR §9.2.2.**
- URL (retrieved): https://nlp.stanford.edu/IR-book/html/htmledition/query-expansion-1.html
- Retrieved verbatim: "for each term in a query, the query can be automatically expanded with
  synonyms and related words"; "one might weight added terms less than original query terms";
  "Use of query expansion generally increases recall and is widely used in many science and
  engineering fields." Four thesaurus types named: controlled vocabularies (e.g. UMLS), manual
  thesauri, automatically derived (co-occurrence) thesauri, query-log mining.
- **Implication:** a *hand-curated alias table* is exactly the "manual thesaurus / controlled
  vocabulary" branch — the highest-precision form. Down-weighting added terms is explicitly
  endorsed. Expansion's primary benefit is **recall**, which fits NestWeaver's code+notes
  retrieval where users phrase queries differently from the indexed identifiers/notes.

### Whole-word / case handling for synonyms
- **Lucene/Elasticsearch:** correct multi-token synonym expansion **must happen at query
  time**, not index time, because a Lucene index cannot store a token graph; query-time
  synonyms are more flexible (no re-index when the table changes) at higher query CPU. Source:
  Elastic "Multi-token Synonyms and Graph Queries" + LUCENE-6664 SynonymGraphFilter
  (https://www.elastic.co/blog/multitoken-synonyms-and-graph-queries-in-elasticsearch ;
  https://lucene.apache.org/core/8_0_0/analyzers-common/org/apache/lucene/analysis/synonym/SynonymGraphFilter.html).
- **Practical rules NestWeaver should adopt:** expand on **whole-token** boundaries after the
  *same* analyzer/tokenizer used at index time (so case-folding and stemming match), apply
  aliases **case-insensitively** by normalizing both sides through the analyzer, and expand at
  **query time** so the alias table can change without re-indexing. The IIR text does not
  specify case rules, so this is grounded in the Lucene practice above, not in IIR.

## 7.2 Prior art / projects (with tradeoffs)

| System | PRF / expansion mechanism | Defaults / notes | Tradeoff |
|---|---|---|---|
| **Indri / Lemur** (origin of RM3 defaults) | Built-in RM3. `RMExpander.cpp`. | fbDocs=10, fbTerms=10, fbOrigWeight=0.5, fbMu=0 (via Anserini #447). | Canonical defaults; C++ ref. |
| **Anserini / Pyserini** (Lucene-based IR research) | `set_rm3(fbDocs, fbTerms, origWeight)`; docvectors index required. | Same 10/10/0.5 defaults. https://github.com/castorini/pyserini/blob/master/docs/usage-interactive-search.md | Validates K=10,N=10,λ=0.5 as the community default. |
| **Lucene SynonymGraphFilter** | Query-time multi-token synonym expansion → token graph → TermAutomatonQuery. | Query-time required for multi-token. | Correct but complex graph machinery; for single-token aliases NestWeaver can stay simpler. |
| **Elasticsearch / OpenSearch** | `synonym`/`synonym_graph` token filters; `query_string` boosts per term (`term^2`); two-pass via `rescore` window. | Per-term boost is the analog of our 0.3x/0.5x weights. https://www.elastic.co/blog/multitoken-synonyms-and-graph-queries-in-elasticsearch | Heavyweight; good template for "expansion terms carry lower boost." |
| **Xapian** | Relevance feedback via ESet + `ExpandDecider` (rejects terms; by default excludes terms already in query). | https://xapian.org/docs/apidoc/html/classXapian_1_1Enquire.html | Shows the "don't re-add terms already in the query" rule we should copy. |
| **Tantivy** (NestWeaver's engine) | BM25 + `BooleanQuery`; per-term `BoostQuery` (multiplicative term boost); no built-in PRF. | https://docs.rs/tantivy/latest/tantivy/query/index.html | We implement PRF/expansion *on top* of Tantivy: pass-1 query, read top-K bodies, mine terms, build a pass-2 `BooleanQuery` with `BoostQuery(0.3)` / `BoostQuery(0.5)` clauses. Fully feasible with existing Tantivy primitives. |

## 7.3 Recommended approach for NestWeaver

1. **Two-pass PRF, bag-of-words (no RM3 LM machinery needed).** Pass 1: original query →
   top-K bodies. Mine candidate terms from those bodies, rank by **high IDF** (rare, content-
   bearing terms; exactly the "mine high-IDF expansion terms" in the RFC). Tantivy exposes the
   doc-frequency stats (`Bm25StatisticsProvider`) to compute IDF cheaply. This is a Rocchio-
   style positive-only expansion (γ=0), which the literature endorses (source A).
2. **Defaults grounded in the literature:**
   - **K (feedback docs) = 10** — the Indri/Anserini/pyserini standard (source C). Solo-dev
     corpora are small, so K=10 is safe; expose as config.
   - **N (expansion terms) = 10** — same standard. Cap hard to avoid query bloat.
   - **Original query weight ≈ 1.0; PRF term weight ≈ 0.3.** RM3 uses λ=0.5 globally, but RM3
     interpolates *language models*; here we keep the user's literal terms at full weight and
     down-weight only the *added* terms. 0.3x for unjudged mined terms is appropriately more
     conservative than Rocchio's β=0.75 because PRF terms are riskier. **This is the RFC's
     number and it is defensible; flag that it is intentionally conservative vs Rocchio.**
   - **Synonym/alias terms ≈ 0.5x** — higher than mined PRF terms (curated ⇒ higher precision)
     but below the user's literal query. Consistent with IIR "weight added terms less."
3. **Curated alias table, expanded at query time, whole-token + case-insensitive** (§7.1E
   rules). Do not re-add terms already present in the query (Xapian's ExpandDecider rule).
   Alias expansion and PRF expansion are independent and can both run; apply alias expansion
   even when PRF is disabled (it is cheaper and lower-risk).
4. **Cap total expanded query length** (RFC requirement). With N≤10 PRF terms + bounded alias
   terms, cap e.g. original + ≤10 PRF + ≤K aliases, then truncate by weight×IDF. Caps bound
   both query drift and per-query latency.
5. **Make PRF opt-in / per-query, alias table always-on but cheap.** PRF doubles BM25 work
   (two passes); for a no-daemon CLI keep it behind a flag/intent so default latency is
   unchanged.
6. **Integration with RRF:** the expanded BM25 query produces a re-ordered BM25 result list;
   feed *that* list into existing RRF (source D). Do not try to push the 0.3/0.5 weights past
   RRF — RRF is rank-only by design.

## 7.4 Pitfalls / failure modes & mitigations
- **Query drift** (the #1 documented PRF failure, source B; the "copper mines → Chile"
  example). Mitigations grounded in literature: (a) **down-weight expansion terms** (0.3x —
  already in design); (b) **cap N** (≤10); (c) **prefer high-IDF terms** so generic words
  don't dominate; (d) keep original query at full weight so the user's intent anchors the
  ranking; (e) consider **selective expansion** — skip PRF when pass-1 top-K looks incoherent
  (low score spread), per the "selective query expansion" line of work (IIR §9 + Springer
  2021). NestWeaver could gate PRF on a pass-1 confidence/score-gap heuristic.
- **Inconsistent gains across queries.** Even when average MAP rises, PRF *hurts some
  individual queries* (retrieved: "PRF benefits are inconsistent across queries," "patchy").
  Mit: opt-in + selective expansion; never make PRF the silent default for every query.
- **Re-adding query terms / stopword expansion.** Mit: exclude existing query terms (Xapian
  rule) and stopwords; IDF thresholding naturally drops stopwords.
- **Synonym analyzer mismatch.** If aliases aren't run through the same analyzer as the index,
  case/stemming mismatches silently drop matches. Mit: normalize both sides via the index
  analyzer; expand at query time.
- **Multi-token aliases.** Single-token aliases are trivial; multi-token correctness needs
  Lucene-style graph handling (source 7.1E). Mit: start with single-token aliases; if multi-
  token is needed, expand into a phrase/`BooleanQuery` subclause rather than naive token
  substitution.
- **Latency from two passes** on a no-daemon CLI. Mit: PRF behind a flag; K small; cache
  pass-1 if the same query repeats.

## 7.5 Complexity / effort & quality signal
- **Effort: MEDIUM.** PRF = second BM25 pass + IDF-ranked term mining + query rebuild with
  weighted `BooleanQuery`/`BoostQuery` (all supported by Tantivy). Alias expansion = a parsed
  table + query-time token rewrite = LOW. The riskiest part is *tuning/gating*, not plumbing.
- **Quality delta (from literature, over weak baselines on TREC/news):** PRF typically lifts
  average MAP on the order of **+10% to +30%** vs an unexpanded query (Springer 2021: 18–35%
  over initial query; RM3 variants 0.5–23% over base RM3). **Caveats:** measured on
  TREC/newswire, over bag-of-words baselines; gains are *inconsistent per query* and can be
  negative without down-weighting/capping. For NestWeaver's code+notes corpus, treat these as
  directional evidence that *conservative, capped, down-weighted* PRF + curated aliases should
  improve recall, and validate with the project's own qualitative top-N checks rather than
  expecting the headline TREC magnitudes.

---

## Source list (all retrieved 2026-05-29)
1. Robertson & Zaragoza 2009, PRF/BM25 monograph — https://dl.acm.org/doi/abs/10.1561/1500000019 ; ref record https://www.scirp.org/reference/referencespapers?referenceid=3896864
2. Robertson, Zaragoza & Taylor 2004, BM25F — https://www.semanticscholar.org/paper/Simple-BM25-extension-to-multiple-weighted-fields-Robertson-Zaragoza/67085d02e3a4710119f1bad050d89c10bd79d977 ; tutorial http://www.minerazzi.com/tutorials/bm25f-model-tutorial.pdf
3. Cormack, Clarke & Büttcher 2009, RRF (SIGIR) — https://dblp.org/rec/conf/sigir/CormackCB09.html ; https://ir.webis.de/anthology/2009.sigirconf_conference-2009.146/ ; DOI 10.1145/1571941.1572114
4. Lavrenko & Croft 2001, Relevance-based LMs — https://www.researchgate.net/publication/221299786_Relevance-based_language_models
5. Manning, Raghavan & Schütze 2008, *IIR* — Rocchio https://nlp.stanford.edu/IR-book/html/htmledition/the-rocchio71-algorithm-1.html ; PRF https://nlp.stanford.edu/IR-book/html/htmledition/pseudo-relevance-feedback-1.html ; query expansion https://nlp.stanford.edu/IR-book/html/htmledition/query-expansion-1.html
6. RM3 operational defaults — Anserini #447 https://github.com/castorini/Anserini/issues/447 ; pyserini https://github.com/castorini/pyserini/blob/master/docs/usage-interactive-search.md
7. PRF quality deltas — Springer Discover Computing 2021 https://link.springer.com/article/10.1007/s10791-021-09393-5
8. Elasticsearch function_score — https://www.elastic.co/guide/en/elasticsearch/reference/current/query-dsl-function-score-query.html
9. OpenSearch function_score — https://docs.opensearch.org/latest/query-dsl/compound/function-score/
10. Vespa ranking — https://docs.vespa.ai/en/ranking/ranking-expressions-features.html ; https://docs.vespa.ai/en/ranking/phased-ranking.html
11. Tantivy — https://docs.rs/tantivy/latest/tantivy/query/index.html ; https://github.com/quickwit-oss/tantivy/blob/main/src/query/bm25.rs
12. Lucene/ES synonyms — https://www.elastic.co/blog/multitoken-synonyms-and-graph-queries-in-elasticsearch ; https://lucene.apache.org/core/8_0_0/analyzers-common/org/apache/lucene/analysis/synonym/SynonymGraphFilter.html
13. Xapian relevance feedback / ExpandDecider — https://xapian.org/docs/apidoc/html/classXapian_1_1Enquire.html

### Verification notes
- `[PDF-NOT-PARSED, corroborated]` items: the RRF SIGIR PDF, the RM3/relevance-models arXiv
  PDFs, and the PRF monograph PDF returned binary that the fetch tool could not convert to
  text. Their *specific facts used here* (RRF formula + k=60; RM3 10/10/0.5 defaults; PRF =
  multiplicative/log-additive prior) are each corroborated by at least one independent
  authoritative record (dblp/IR-Anthology bibliographic entries, Indri source via Anserini,
  pyserini API) and were not taken from the unparsed PDFs.
- Brin & Page PageRank cited as background framing only `[UNVERIFIED exact wording]`; the
  "PageRank as query-independent prior" claim is restated in source 1.
