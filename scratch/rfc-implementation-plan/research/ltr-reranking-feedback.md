# Research Foundation: Agent Feedback Loop (Feature 1) & Lightweight Learned Listwise Reranker (Feature 17)

Compiled for NestWeaver (Rust code-and-notes graph intelligence: tree-sitter indexing, embedded graph DB, Personalized PageRank + BM25/Tantivy + optional vectors fused via RRF, MCP server, solo-dev / no-GPU / no-daemon).

All sources below were retrieved via web search/fetch on 2026-05-29. Where a quantitative claim could not be confirmed from the retrieved page text it is marked `[UNVERIFIED]`. No papers, authors, venues, or results are fabricated.

---

## Cross-cutting context: NestWeaver is NOT a web search engine

Most implicit-feedback / click-model / unbiased-LTR theory was developed for *web search with high-volume traffic and a SERP layout*. NestWeaver differs on every axis that matters:

- **One user (solo dev), low query volume.** IPS-based unbiased LTR and click models are statistically hungry — they assume thousands+ of impressions per query/position to estimate propensities or model parameters. NestWeaver will never have that. This *fundamentally bounds* how much of the unbiased-LTR machinery is applicable; it should inform design (favor robust heuristics + tiny models + conservative gates) rather than be copied wholesale.
- **The "presentation" is an MCP tool result or CLI output**, often consumed by an LLM agent, not a human scanning a ranked SERP top-to-bottom. Position bias still plausibly exists (agents/humans attend to earlier results), but cascade-style "examine until satisfied then abandon" is a weaker assumption.
- **A genuine, observable success signal exists that web search lacks**: the agent subsequently *edits/writes a file* (acts on the retrieved symbol). This is closer to a conversion/purchase signal than a click — far stronger than dwell time.

These differences are the spine of the recommendations below.

---

# FEATURE 1 — Agent Feedback Loop (interaction events → PPR teleport boost; safe success signal)

## 1. Research foundation

### Implicit feedback as a relevance signal (and its biases)

- **Fox, Karnawat, Mydland, Dumais, White (2005). "Evaluating implicit measures to improve web search." ACM TOIS 23(2):147–168.** Retrieved: https://dl.acm.org/doi/10.1145/1059981.1059982 (full PDF: http://susandumais.com/tois-p147-fox.pdf).
  - **Result that matters:** There *is* a measurable association between implicit signals and explicit user satisfaction, and the best predictive models combine **clickthrough, dwell time on the result, and how the user exited the result / ended the session.** Crucially, *exit type / session-end behavior* carries signal — directly supports NestWeaver's "end-of-session without further searching = success" idea.

- **Craswell, Zoeter, Taylor, Ramsey (2008). "An experimental comparison of click position-bias models." WSDM 2008, pp. 87–94.** Retrieved: https://dl.acm.org/doi/10.1145/1341531.1341545 (PDF mirror: https://www.researchgate.net/publication/200110550).
  - **Result that matters:** Clicks are strongly biased by **presentation position**; a **cascade model** (user scans top→bottom, clicks the first worthwhile result, then stops) best explains position bias at early ranks. Implication for NestWeaver: a result at rank 1 getting "used" is partly an artifact of being shown first, not purely of being more relevant. Any teleport boost derived from interaction counts must account for this or it will entrench whatever was already ranked highly.

- **Chuklin, Markov, de Rijke (2015). "Click Models for Web Search." Synthesis Lectures on Information Concepts, Retrieval, and Services, Morgan & Claypool. DOI 10.2200/S00654ED1V01Y201507ICR043.** Retrieved: https://link.springer.com/book/10.1007/978-3-031-02294-4 (authors' PDF: https://clickmodels.weebly.com/uploads/5/2/2/5/52257029/mc2015-clickmodels.pdf; SIGIR'15 tutorial: https://irlab.science.uva.nl/wp-content/papercite-data/pdf/chuklin-introduction-2015.pdf).
  - **Result that matters:** Canonical reference cataloguing click models — Position-Based Model (PBM), Cascade Model, Dependent Click Model (DCM), User Browsing Model (UBM), Dynamic Bayesian Network (DBN). Key takeaway for us: separating *examination* (was it seen?) from *attractiveness/relevance* (was it good?) requires a model of how results are presented and consumed. NestWeaver lacks the data to fit any of these properly, so we should borrow the *conceptual decomposition* (examination ≠ relevance) rather than the estimators.

### Counterfactual / unbiased learning-to-rank from implicit feedback

- **Joachims, Swaminathan, Schnabel (2017). "Unbiased Learning-to-Rank with Biased Feedback." WSDM 2017 (preprint arXiv:1608.04468, 2016).** Retrieved: https://arxiv.org/abs/1608.04468 (PDF: https://www.cs.cornell.edu/~tj/publications/joachims_etal_17a.pdf).
  - **Result that matters:** Provides a **counterfactual / Empirical-Risk-Minimization framework** for unbiased LTR despite biased clicks: weight each observed click by the **inverse of its examination propensity (Inverse Propensity Scoring, IPS)**, yielding a Propensity-Weighted Ranking SVM. The framework is *provably unbiased* w.r.t. the true relevance if propensities are correct, and is shown robust to noise and propensity misspecification; reported to substantially improve retrieval on an operational engine (specific magnitude not in abstract `[UNVERIFIED exact %]`).
  - **For NestWeaver:** IPS is the theoretically correct way to keep a feedback loop honest — but it needs per-position propensity estimates that require volume we won't have. The *actionable lesson* is conceptual: **never feed raw interaction counts straight back into ranking; down-weight signal coming from already-high positions.** Even a crude static position-discount approximates IPS's intent.

- **Ai, Bi, Luo, Guo, Croft (2018). "Unbiased Learning to Rank with Unbiased Propensity Estimation." SIGIR 2018.** Retrieved: https://arxiv.org/pdf/1804.05938 (ACM: https://dl.acm.org/doi/10.1145/3209978.3209986).
  - **Result that matters:** Shows propensities can be **estimated jointly from click data (Dual Learning Algorithm)** without separate position-swap experiments, making unbiased LTR more practical. Confirms the direction but, again, is data-hungry.

### Feedback-loop / runaway bias (the danger)

- **Ensign, Friedler, Neville, Scheidegger, Venkatasubramanian (2018). "Runaway Feedback Loops in Predictive Policing." FAT* 2018, PMLR 81 (preprint arXiv:1706.09847).** Retrieved: https://arxiv.org/abs/1706.09847 (PDF: https://friedler.net/papers/feedbackloops_fat18.pdf; PMLR: https://proceedings.mlr.press/v81/ensign18a.html).
  - **Result that matters:** Formal (Pólya-urn) proof that when a system's *future training data is collected only from where the model already directs attention*, sampling bias compounds into a **runaway feedback loop** — the model keeps reinforcing its prior choices regardless of ground truth. They show the fix is to **adjust the inputs in a black-box way (discount discoveries by how much attention drove them).** This is the single most important cautionary result for Feature 1: an interaction-count → teleport-boost loop is structurally identical to the policing loop (we only get interactions on results we surfaced).

### Personalization vector in PageRank (the mechanism)

- **Haveliwala (2002). "Topic-Sensitive PageRank." WWW 2002, pp. 517–526.** Retrieved: https://dl.acm.org/doi/10.1145/511446.511513 (PDF: https://www.cs.cmu.edu/~christos/courses/826-resources/PAPERS+BOOK/Haveliwala_www2003.pdf).
  - **Result that matters:** PageRank can be biased toward a topic/context by replacing the uniform teleport (jump) distribution with a **nonuniform personalization vector**; the random-surfer restart probability mass is redistributed to favored nodes, increasing their rank and the rank of nodes they point to. This is *exactly* the established, principled mechanism NestWeaver's PPR already uses for seeds, and exactly where a feedback boost belongs. The boost should modulate the **teleport/restart vector**, not edge weights or final scores — consistent with both Haveliwala and standard Personalized PageRank.

## 2. Prior art / projects

| System | Approach | Tradeoffs / relevance to NestWeaver |
|---|---|---|
| Cornell `svm_proprank` (Propensity SVM-rank) | Reference impl of IPS-weighted LTR from Joachims et al. | https://www.cs.cornell.edu/people/tj/svm_light/svm_proprank.html — proves the method, but C, SVM, and needs propensities. Reference only. |
| Web-search click-log pipelines (Google/Bing-style, per Craswell/Chuklin) | Fit click models (DBN/UBM) to massive logs, derive relevance | Not viable for single-user volume. Borrow concepts only. |
| RecSys exposure-bias literature (generalization of Ensign et al.) | Document that naive "log → train → serve → log" loops cause popularity entrenchment | Confirms runaway risk applies to any self-collected-feedback ranker. |

## 3. Recommended approach for NestWeaver (grounded)

**Mechanism (where the boost goes).** Apply the interaction score as a **multiplicative factor on the per-UID teleport (restart) mass of the existing Personalized PageRank**, capped at 2.0x as specified. This is the Haveliwala personalization-vector mechanism and keeps the boost inside the well-understood PPR fixed point rather than hacking final scores. Renormalize the teleport vector after boosting so it remains a probability distribution.

**Defining a SAFE success signal (the core of the feature).** Grounded in Fox et al. (2005) — exit type and session-end behavior predict satisfaction — adopt a *sequence-aware* labeling of each surfaced result:

- **NEGATIVE / no-success:** the result was surfaced and the agent's *next action within the session is another search/context query* (especially a reformulation). This mirrors the cascade/"abandon and look again" pattern: re-querying signals the prior result did not satisfy. **Do not award success credit here.**
- **POSITIVE / success:** the surfaced UID (or a UID in the same file/symbol neighborhood) is followed by a **write/edit/impact action on that symbol**, OR the **session ends with no further search** after the result was accessed. The write/edit signal is NestWeaver's analogue of a *conversion* — far stronger than a click and not available to web search. Weight it highest.
- **WEAK / neutral:** mere access (open/read) with neither of the above — treat as low-weight or zero, because access alone is the most position-biased and least reliable signal (Craswell et al.).

This is a deliberately simple, robust heuristic rather than a fitted click model, which is the correct altitude for single-user data volume.

**Avoiding feedback-loop bias (mandatory, per Ensign et al.).** Because we only observe interactions on results we surfaced, build in explicit dampers:

1. **Position discounting (poor-man's IPS).** Discount the success credit of a result by a static, decreasing function of the rank at which it was shown (e.g. credit ∝ 1/log2(1+rank), or a fixed examination-probability table). This approximates IPS's "down-weight signal that came from a privileged position" without needing to estimate propensities — directly the Joachims et al. intent and the Ensign et al. fix.
2. **Cap the boost (already specified: 2.0x).** A hard multiplicative ceiling bounds runaway growth.
3. **Time-decay (already in the sidecar).** Decay keeps stale entrenchment from accumulating; ensures the loop "forgets."
4. **Exploration floor.** Never let the boost drive a result's effective teleport mass so high that genuinely-relevant-but-never-surfaced nodes can't appear. Keep a minimum uniform teleport component (this is the structural fix Ensign et al. prescribe — guarantee non-zero exploration).
5. **Boost relevance, not just popularity.** Prefer crediting the *success* signal (edit/write/session-end) over raw access frequency, so the loop reinforces *useful* results, not merely *frequently-shown* ones.

**Why not full IPS/click models here:** insufficient per-position impression volume for a single user to estimate propensities or fit DBN/UBM. The honest engineering answer is to take the *concepts* (examination ≠ relevance; discount privileged positions; guarantee exploration) and implement them as cheap deterministic rules.

## 4. Pitfalls / failure modes + mitigations

| Pitfall | Source grounding | Mitigation |
|---|---|---|
| Runaway feedback loop: surfaced→used→boosted→surfaced more, regardless of true relevance | Ensign et al. 2018 | Position discounting + 2.0x cap + time-decay + uniform exploration floor |
| Position-bias entrenchment (rank-1 results win just for being rank 1) | Craswell et al. 2008 | Discount credit by show-position (poor-man's IPS) |
| Treating mere access as success | Fox et al. 2005 (access weak; exit/dwell stronger) | Tiered signal: weight write/edit and clean session-end above raw access |
| Mislabeling reformulation as success | cascade abandonment intuition | "next action is another search ⇒ NOT success" rule |
| Over-personalizing to recent noise / one bad session | general | Time-decay (have it) + cap; require a minimum count before any boost applies |
| Cold start: no interactions yet | — | Boost defaults to 1.0x (identity); PPR behaves exactly as today |

## 5. Complexity / effort + expected deltas

- **Effort:** Low–Medium. Sidecar event recording exists. New work: (a) sequence-aware success labeler over the event stream (next-action rule + session boundary detection), (b) position-discounted scoring, (c) teleport-vector boost+renormalize in the existing PPR pass, (d) the four dampers. All pure Rust, no model, no GPU.
- **Expected quality delta:** No directly transferable published number — the personalization literature is web-scale and multi-user. Treat improvement as **plausible but unproven for single-user**; gate via the same offline nDCG harness built for Feature 17 (below). The dominant *risk-reduction* deliverable here is the anti-feedback-loop design, not a headline metric.

---

# FEATURE 17 — Lightweight Learned Listwise Reranker (~20K params, ~100KB, candle, top-50, gated ≥5% nDCG@10)

## 1. Research foundation

### Learning-to-rank: pointwise / pairwise / listwise

- **Burges (2010). "From RankNet to LambdaRank to LambdaMART: An Overview." Microsoft Research Technical Report MSR-TR-2010-82.** Retrieved: https://www.microsoft.com/en-us/research/wp-content/uploads/2016/02/MSR-TR-2010-82.pdf.
  - **Result that matters:** Definitive, self-contained derivation of the pairwise→listwise-flavored family. **RankNet** = pairwise cross-entropy on score differences (neural). **LambdaRank** = define the gradient (λ) directly, scaling each pairwise gradient by the **|ΔnDCG|** that swapping the pair would cause — this optimizes a smooth surrogate aligned with the (non-differentiable) nDCG metric. **LambdaMART** = LambdaRank gradients in gradient-boosted trees; an ensemble of LambdaMART **won Track 1 of the 2010 Yahoo! Learning-to-Rank Challenge.** This is the empirically dominant LTR recipe and the natural baseline-of-comparison.

- **Cao, Qin, Liu, Tsai, Li (2007). "Learning to Rank: From Pairwise Approach to Listwise Approach." ICML 2007 (24th ICML).** Retrieved (MSR tech-report PDF tr-2007-40): https://www.microsoft.com/en-us/research/wp-content/uploads/2016/02/tr-2007-40.pdf (ACM: https://dl.acm.org/doi/abs/10.1145/1390156.1390306).
  - **Result that matters:** Introduces the **listwise approach** and **ListNet**: loss = **cross-entropy between two permutation-probability distributions** (top-one probability under Placket–Luce), one from predicted scores, one from ground-truth labels. Listwise losses model the whole ranked list jointly rather than isolated pairs/points, and the paper shows listwise outperforms pairwise on benchmark IR data.

- **Xia, Liu, Wang, Zhang, Li (2008). "Listwise Approach to Learning to Rank: Theory and Algorithm." ICML 2008.** Retrieved: https://icml.cc/Conferences/2008/papers/167.pdf.
  - **Result that matters:** Introduces **ListMLE** (maximum likelihood over the observed permutation via Plackett–Luce), with a theoretical analysis of listwise loss consistency. ListMLE is simpler/cheaper than ListNet's full cross-entropy and is a good fit for a tiny model. Confirms listwise losses are theoretically sound surrogates for ranking metrics.

### Evaluation metric (the gate)

- **Järvelin, Kekäläinen (2002). "Cumulated Gain-based Evaluation of IR Techniques." ACM TOIS 20(4):422–446.** Retrieved: https://dl.acm.org/doi/10.1145/582415.582418 (PDF: https://faculty.cc.gatech.edu/~zha/CS8803WST/dcg.pdf).
  - **Result that matters:** Defines **(n)DCG** — discounts gain by log of rank position and normalizes against the ideal ranking, supporting **graded relevance** and emphasizing highly-relevant docs near the top. This is the canonical justification for using **nDCG@10** as Feature 17's promotion gate.

### Neural rerankers (cross-encoders) — for calibration of expectations

- **Nogueira, Cho (2019). "Passage Re-ranking with BERT." arXiv:1901.04085.** Retrieved: https://arxiv.org/abs/1901.04085 (PDF: https://arxiv.org/pdf/1901.04085).
  - **Result that matters:** The "monoBERT" cross-encoder reranker over top-k first-stage candidates set state-of-the-art on TREC-CAR and topped the MS MARCO passage leaderboard, **+27% relative MRR@10 over the prior best** (per the paper's reported headline). Establishes the now-standard **retrieve-then-rerank** architecture and that reranking the *top-k* (not the whole corpus) is where rerankers pay off — directly validating NestWeaver's "rerank top-50" design. **Caveat:** monoBERT is ~110M params; its gains do NOT transfer to a 20K-param feature-based model. Cited to justify *the architecture*, not the magnitude.

- **Sentence-Transformers cross-encoders (e.g. `cross-encoder/ms-marco-MiniLM-L-6-v2`).** Retrieved: https://www.sbert.net/docs/cross_encoder/pretrained_models.html and https://sbert.net/docs/cross_encoder/training_overview.html.
  - **Result that matters:** Documents the standard **retrieve-and-rerank** pattern (fast first stage retrieves X candidates; cross-encoder reranks the top X) and explicitly notes you should **experiment to confirm the slower second stage is worth it** — i.e. a reranker is not always a win; latency must be justified. This is the practitioner grounding for Feature 17's ≥5% gate.

## 2. Prior art / projects

| Project | What it is | Tradeoffs / relevance |
|---|---|---|
| **OpenSearch Learning to Rank plugin** | In-engine LTR; logs query-dependent features, serves XGBoost/RankLib models (LambdaMART, RF). Training is *offline/external*. | https://docs.opensearch.org/latest/search-plugins/ltr/index/ — Validates NestWeaver's exact split: **online feature logging + scoring, offline training.** Tree models, not neural. |
| **Elasticsearch LTR** | Same lineage (RankLib/XGBoost serialized models uploaded to engine). | https://elasticsearch-learning-to-rank.readthedocs.io/en/latest/training-models.html — Same pattern. |
| **Vespa ranking** | Multi-phase ranking: cheap first-phase, expensive second/global-phase. Imports XGBoost GBDT and **ONNX** models; warns large models belong in later phases. | https://docs.vespa.ai/en/ranking/phased-ranking.html , https://docs.vespa.ai/en/ranking/onnx.html — Strong precedent for **phased ranking** (hybrid first stage → learned reranker on top-50). |
| **XGBoost `rank:ndcg` / LightGBM `lambdarank`** | Off-the-shelf LambdaMART-family rankers. | https://xgboost.readthedocs.io/en/stable/tutorials/learning_to_rank.html — The pragmatic baseline; trees often beat tiny nets on tabular features. Worth training as a comparison even if the *shipped* model is candle. |
| **candle (huggingface)** | Minimalist Rust ML framework, PyTorch-like tensors, CPU (MKL/Accelerate)/CUDA/WASM, inference-focused, `candle-onnx` for ONNX eval. | https://github.com/huggingface/candle , https://github.com/huggingface/candle/tree/main/candle-onnx — A ~20K-param MLP/linear listwise scorer is trivially within candle's CPU capabilities; no GPU needed; aligns with no-daemon/no-GPU constraint. |
| **ONNX-in-Rust (ort / candle-onnx / tract)** | Run models trained in Python (XGBoost→ONNX, or a torch MLP→ONNX) inside Rust. | Lets you train with mature Python tooling, ship a tiny ONNX in pure Rust. Reduces "train in Rust" risk. |

## 3. Recommended approach for NestWeaver (grounded)

**Architecture: phased ranking, validated by monoBERT/Vespa/SBERT.** Keep the hybrid (PPR + BM25 + optional vectors via RRF) as the **first stage**; the learned model **reranks only the top-50** candidates. Reranking a small candidate set is exactly where the literature shows rerankers pay off and where cost stays bounded.

**Features (as specified) are sound and tabular:** `[rank_position, bm25_score, ppr_score, node_kind_onehot, is_inline_body, age_days, matched_alias_count]`. Note `rank_position` and `bm25_score`/`ppr_score` encode first-stage signal; including `rank_position` lets the model learn a residual over the baseline (helpful) but **also re-imports position bias** — keep it but be aware (see pitfalls). All features are cheap to compute at query time, no embeddings required for the reranker itself.

**Model + loss.** A ~20K-param model on ~7 features is **credible**: with one-hot node kinds the input is ~10–20 dims; a small MLP (e.g. 20→64→32→1 ≈ 3–4K params, or wider to reach ~20K) is ample — these features are low-dimensional and largely monotone, so capacity is not the bottleneck (this is *under-parameterized-for-NLP* but *appropriately-sized for tabular ranking*; cf. trees winning Yahoo LTR with modest models). Use a **listwise loss**: **ListMLE** (Xia et al. 2008) is the simplest to implement and cheap; **ListNet** (Cao et al. 2007) or a **LambdaRank-style λ weighting by |ΔnDCG|** (Burges 2010) are the metric-aligned alternatives. Recommend starting with **ListNet/ListMLE** for simplicity and adding LambdaRank weighting only if the gate isn't met.

**Honest baseline check first.** Per the LTR literature, **train an XGBoost/LightGBM LambdaMART model on the same features+labels** as a yardstick. If trees beat the tiny candle net by a wide margin, ship the tree (export to ONNX, run via `candle-onnx`/`ort`) — the deliverable is *a reranker that clears the gate*, not specifically a hand-rolled net.

**Offline training from implicit labels.** Derive graded relevance labels from Feature 1's success signal:
- label 2 (highly relevant): result followed by edit/write on that symbol (the conversion signal);
- label 1: accessed and session ended cleanly without re-query;
- label 0: surfaced-but-ignored, or followed immediately by another search.
**Apply position-discounting / IPS-style reweighting to these labels** (Joachims et al. 2017) so the reranker doesn't just learn to reproduce the first-stage order. Train offline, batch, from the interaction sidecar — exactly the OpenSearch/Elasticsearch "offline training, online serving" split.

**The gate is sound.** Promote the reranker **only if it beats the hybrid baseline by ≥5% nDCG@10** on a held-out set. nDCG@10 is the right metric (Järvelin & Kekäläinen). Recommended hardening of the gate:
- evaluate via **time-based or query-based cross-validation / held-out split**, not random row shuffling (avoids leaking session structure);
- because single-user label volume is tiny, report a **confidence interval / per-query win-loss count**, not just a point estimate — a 5% mean lift on 40 queries can be noise. Consider also reporting **MRR** as a secondary check.
- ship behind a flag, default off, so a non-improving model never degrades the product.

## 4. Pitfalls / failure modes + mitigations

| Pitfall | Source grounding | Mitigation |
|---|---|---|
| Reranker just relearns first-stage order (no real lift) | retrieve-rerank caveat, SBERT docs | IPS/position-discounted labels (Joachims 2017); the ≥5% gate kills no-lift models |
| `rank_position` feature re-imports position bias into labels AND features | Craswell 2008; Joachims 2017 | Discount labels by show-position; consider training a variant *without* rank_position to measure its contribution |
| Tiny training set → overfit; 5% lift is noise | LTR practice | Time/query-based CV, report CI / per-query wins, secondary MRR, default-off flag |
| Reranker not worth its latency | SBERT "experiment if the 2nd stage is worth it" | Top-50 only; tiny model (~100KB) → sub-millisecond CPU inference; the gate is also implicitly a cost/benefit check |
| Distribution shift: codebase changes, labels go stale | feedback-loop literature | Retrain periodically; time-decay labels (shared with Feature 1) |
| Hand-rolled candle net underperforms a tree | Yahoo LTR / XGBoost-rank dominance on tabular | Train XGBoost LambdaMART baseline; ship whichever clears the gate (ONNX via candle-onnx/ort if it's the tree) |
| Over-relying on web-search reranker gains (monoBERT +27%) | Nogueira & Cho 2019 (110M params) | Do NOT expect such magnitudes from 20K params; cite for architecture only |

## 5. Complexity / effort + reported quality deltas

- **Effort:** Medium. Feature extraction at query time (cheap, reuses existing scores). Offline training harness + label derivation (depends on Feature 1's success labeler — build that first). candle MLP + ListMLE/ListNet loss is small; an XGBoost baseline is near-free. nDCG@10 eval harness with proper CV is the most careful part.
- **Reported quality deltas (calibration, not transfer):**
  - Cross-encoder reranking over a hybrid first stage: **+27% relative MRR@10** for monoBERT on MS MARCO (Nogueira & Cho 2019) — *upper bound for 110M-param models; not a target for 20K params.*
  - LambdaMART: state-of-the-art tabular LTR, won the 2010 Yahoo! LTR Challenge Track 1 (Burges 2010) — realistic family for our feature set.
  - Listwise > pairwise on LETOR benchmarks (Cao et al. 2007) — supports the listwise choice.
  - **For NestWeaver specifically: no transferable number exists** (single-user, code-graph domain). The **≥5% nDCG@10 gate is the empirical contract** — if the model doesn't clear it on held-out queries with a reasonable confidence interval, it doesn't ship. This is the correct, defensible posture.

---

## Source index (all retrieved 2026-05-29)

1. Fox et al. 2005, TOIS — implicit measures / dwell / exit: https://dl.acm.org/doi/10.1145/1059981.1059982 (PDF http://susandumais.com/tois-p147-fox.pdf)
2. Craswell et al. 2008, WSDM — click position-bias / cascade: https://dl.acm.org/doi/10.1145/1341531.1341545
3. Chuklin, Markov, de Rijke 2015 — Click Models for Web Search: https://link.springer.com/book/10.1007/978-3-031-02294-4 (PDF https://clickmodels.weebly.com/uploads/5/2/2/5/52257029/mc2015-clickmodels.pdf)
4. Joachims, Swaminathan, Schnabel 2017, WSDM — Unbiased LTR / IPS: https://arxiv.org/abs/1608.04468 (PDF https://www.cs.cornell.edu/~tj/publications/joachims_etal_17a.pdf)
5. Ai et al. 2018, SIGIR — Unbiased propensity estimation: https://arxiv.org/pdf/1804.05938
6. Ensign et al. 2018, FAT* — Runaway Feedback Loops: https://arxiv.org/abs/1706.09847 (PDF https://friedler.net/papers/feedbackloops_fat18.pdf)
7. Haveliwala 2002, WWW — Topic-Sensitive PageRank: https://dl.acm.org/doi/10.1145/511446.511513
8. Burges 2010, MSR-TR-2010-82 — RankNet/LambdaRank/LambdaMART: https://www.microsoft.com/en-us/research/wp-content/uploads/2016/02/MSR-TR-2010-82.pdf
9. Cao et al. 2007, ICML — ListNet: https://www.microsoft.com/en-us/research/wp-content/uploads/2016/02/tr-2007-40.pdf
10. Xia et al. 2008, ICML — ListMLE: https://icml.cc/Conferences/2008/papers/167.pdf
11. Järvelin & Kekäläinen 2002, TOIS — (n)DCG: https://dl.acm.org/doi/10.1145/582415.582418
12. Nogueira & Cho 2019 — Passage Re-ranking with BERT (monoBERT): https://arxiv.org/abs/1901.04085
13. Sentence-Transformers cross-encoder docs: https://www.sbert.net/docs/cross_encoder/pretrained_models.html ; https://sbert.net/docs/cross_encoder/training_overview.html
14. OpenSearch LTR plugin: https://docs.opensearch.org/latest/search-plugins/ltr/index/
15. Elasticsearch LTR: https://elasticsearch-learning-to-rank.readthedocs.io/en/latest/training-models.html
16. Vespa phased ranking / ONNX: https://docs.vespa.ai/en/ranking/phased-ranking.html ; https://docs.vespa.ai/en/ranking/onnx.html
17. XGBoost learning-to-rank: https://xgboost.readthedocs.io/en/stable/tutorials/learning_to_rank.html
18. candle + candle-onnx: https://github.com/huggingface/candle ; https://github.com/huggingface/candle/tree/main/candle-onnx
