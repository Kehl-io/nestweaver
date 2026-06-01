# Research Foundation: Feature 12 (Git-Activity-Dampened CodeRank) & Feature 13 (affected_tests)

> Compiled 2026-05-29 for NestWeaver RFC planning. All cited sources were retrieved via web
> search/fetch; URLs are listed inline. Claims that could not be verified from a primary
> source are marked **[UNVERIFIED]**. Quantitative figures are attributed to the specific
> source that reported them; do not generalize them beyond their study context.

---

## FEATURE 12 — Git-Activity-Dampened CodeRank

**Design under evaluation:** apply a per-file decay multiplier to PageRank at compute time.
`recency_score = mean over last N=20 touching commits of exp(-Δdays/180)`, skipping
bulk/mechanical commits (≥500 files). Final multiplier
`(1 + activity_weight·(score − 0.5))` clamped to `[0.4, 1.6]`, `activity_weight` default 0.6.
Goal: dormant code ranks below active code of the same name across many repos.

### 12.1 Research foundation (primary sources)

**(a) Temporal / time-aware link analysis — the core precedent for decaying a graph-centrality score.**

- **Rozenshtein & Gionis, "Temporal PageRank," ECML-PKDD 2016.**
  Springer: https://link.springer.com/chapter/10.1007/978-3-319-46227-1_42 ·
  PDF: http://users.ics.aalto.fi/gionis/temporal-pagerank.pdf
  *Result / idea:* generalizes PageRank to temporal networks where activity is a sequence of
  time-stamped edges, via a "temporal random walk." Processes edges in arrival order; proven to
  converge to temporal PageRank scores, and — importantly for us — **if the edge distribution
  remains constant, temporal PageRank converges to the static PageRank of the underlying graph.**
  This is the formal license for "decay toward baseline": a temporal weighting that degrades
  gracefully to ordinary PageRank when there is no temporal signal.

- **Berberich, Vazirgiannis, Weikum — "T-Rank: Time-Aware Authority Ranking" (link analysis with
  *freshness* = timestamp of most recent update, and *activity* = update rate).** Discussed across
  the temporal-ranking literature surveyed at
  https://informationr.net/ir/18-3/paper586.html ("The impact of time in link-based Web ranking").
  *Result / idea:* incorporating freshness and update-rate of nodes/links into authority ranking
  produces rankings closer to human judgment than static PageRank. Establishes "recency of last
  touch" and "rate of change" as the two canonical temporal signals — exactly the two signals git
  history exposes (last-touched, churn rate).

- **"TimedPageRank" / time-weighted authoritative ranking (bibliographic/citation setting).**
  Springer "Time-weighted web authoritative ranking":
  https://link.springer.com/article/10.1007/s10791-010-9138-4 ;
  Yu, Li & Liu, "Adding the Temporal Dimension to Search — A Case Study in Publication Search" (WI 2005):
  https://www.cs.uic.edu/~liub/publications/WI-05-temp.pdf
  *Result / idea:* TimedPageRank weights each citation by an **exponential decay function of citation
  age**, with an aging factor so node scores decline with time. Empirically, adding the simple
  age-decay term **consistently improved retrieval performance**. This is direct precedent for using
  `exp(-Δt/τ)` as the decay kernel — the same kernel Feature 12 proposes.

> **Synthesis for Feature 12.** Feature 12 is not novel in kind; it is "freshness/activity-aware
> link analysis" (T-Rank, TimedPageRank) applied to a code-call graph. The literature supports:
> (i) an exponential age-decay kernel; (ii) using both last-touch recency and update-rate;
> (iii) graceful degradation to plain PageRank when temporal signal is flat (Temporal PageRank).

**(b) Code churn / change history as a *quality/importance* signal — justifies that git activity
carries real information about code, not noise.**

- **Nagappan & Ball, "Use of Relative Code Churn Measures to Predict System Defect Density," ICSE 2005,
  pp. 284–292.**
  Abstract/MSR: https://www.microsoft.com/en-us/research/publication/use-of-relative-code-churn-measures-to-predict-system-defect-density/ ·
  ACM: https://dl.acm.org/doi/10.1145/1062455.1062514 · PDF:
  https://www.microsoft.com/en-us/research/wp-content/uploads/2016/02/icse05churn.pdf
  *Result (from the abstract):* case study on **Windows Server 2003**. **Absolute** churn measures
  (raw LOC changed) are *poor* predictors of defect density; a set of **relative** churn measures
  (churn normalized by component size and by temporal extent) is **highly predictive** of defect
  density. The metric suite discriminated fault-prone vs. non-fault-prone binaries with ~**89%**
  accuracy. *Implication for us:* (1) git activity is a legitimate signal; (2) **normalize churn**
  (relative, not absolute) — raw "lines/files changed" is the weak version. Feature 12's per-file
  averaging over a fixed window of commits is a relative/normalized formulation, which is the
  defensible side of this finding.

- **Hassan, "Predicting Faults Using the Complexity of Code Changes," ICSE 2009, pp. 78–88.**
  ACM: https://dl.acm.org/doi/10.1109/ICSE.2009.5070510 · PDF:
  https://sailresearch.github.io/sail-website/data/pdfs/ICSE2009_PredictingFaultsUsingTheComplexityOfCodeChanges.pdf
  *Result:* uses **Shannon entropy of the change process** (how spread-out modifications are across
  files in a period) as a complexity metric. Across six large OSS projects, these change-complexity
  metrics are **better fault predictors than prior modifications or prior faults alone.** *Implication:*
  the *distribution/spread* of changes matters, not just the count — a refinement we can adopt later
  (entropy-weighted activity) but is out of scope for the v1 decay formula.

**(c) Adoption / human-factors caveat — the warning that a working signal can still fail in practice.**

- **Lewis, Lin, Sadowski, Zhu, Ou & Whitehead, "Does Bug Prediction Support Human Developers?
  Findings from a Google Case Study," ICSE 2013, pp. 372–381.**
  Google: https://research.google/pubs/does-bug-prediction-support-human-developers-findings-from-a-google-case-study/ ·
  ACM: https://dl.acm.org/doi/10.5555/2486788.2486838
  *Result:* deploying an academically-validated bug-prediction algorithm at Google produced **no
  identifiable change in developer behavior.** *Implication for us:* a churn/recency signal that is
  statistically valid may still be ignored or distrusted if it's not legible. Feature 12 should make
  the multiplier **explainable** (show the recency_score and which commits drove it), and keep its
  influence bounded (the clamp) so it never produces inexplicable rank flips.

### 12.2 Recommended approach for NestWeaver (grounded)

1. **Keep the exponential decay kernel** `exp(-Δdays/τ)`, τ=180d. This matches TimedPageRank's
   age-decay kernel. τ=180d gives a half-life of `180·ln2 ≈ 125 days` — i.e. code untouched ~4
   months is weighted ~0.5. That is a reasonable "quarter-ish" freshness horizon for source code;
   defensible but a tunable, not a derived-from-theory constant (state it as a chosen prior).

2. **Compute-time multiplier, not edge rewrite.** Temporal PageRank rewrites the walk; that is
   expensive and changes convergence. Feature 12's *post-hoc per-node multiplier* is the pragmatic
   T-Rank-style "freshness reweighting of authority." This is sound **as long as it is applied as a
   re-rank/scaling, not fed back into the iterative PageRank fixpoint** (which could destabilize
   convergence and the `<db>.pagerank.json` cache semantics). Recommend: store raw PageRank as today,
   apply multiplier at query/rank time.

3. **Center at 0.5 and clamp — keep the "degrade to baseline" property.** `(1 + 0.6·(score−0.5))`
   maps score=0.5 → 1.0 (no change), and the `[0.4,1.6]` clamp bounds the swing to ±60%. This mirrors
   Temporal PageRank's guarantee that with flat temporal signal you recover static PageRank: a file
   with median activity is unchanged. Good. **Verify the score is bounded in [0,1]** (it is: mean of
   `exp(-x)` terms each in (0,1]), so the multiplier naturally lands in `[1−0.3, 1+0.3]=[0.7,1.3]`
   before clamping — meaning the **[0.4,1.6] clamp is wider than the formula can ever reach**. Either
   tighten the clamp to ~`[0.7,1.3]` to match reality, or raise `activity_weight`'s allowed max so the
   clamp is the true limiter. Flag this as a spec inconsistency to resolve in the RFC.

4. **Normalize, per Nagappan & Ball.** Averaging `exp(-Δt)` over a fixed-count window (N=20) is
   already a relative/normalized measure (recency, not raw volume) — keep it. Do **not** switch to raw
   commit counts or raw LOC, which the paper shows are weak.

5. **Bulk-commit filtering is essential** (see 12.3 and the MSR commit-size literature below).

### 12.3 Pitfalls / failure modes + mitigations

| Pitfall | Evidence / reasoning | Mitigation |
|---|---|---|
| **Churn ≠ quality / importance.** High churn correlates with *defects* (Nagappan & Ball), not with code being *more important to retrieve*. Feature 12 boosts active code, but active≠good. | Nagappan & Ball, Hassan. | Frame the multiplier as **recency/relevance for retrieval**, not a quality score. Keep influence bounded (clamp). Don't surface it as "code health." |
| **Bulk / mechanical commits poison recency** (reformat, license-header sweeps, dependency bumps, generated-code regen, mass-rename) make dormant files look freshly active. | Hindle, German & Holt, "What do large commits tell us?" MSR 2008 (https://dl.acm.org/doi/10.1145/1370750.1370773 ; author page https://softwareprocess.es/homepage/papers/2008-abram2008msr08wdlctuatsolc/): large commits skew **perfective** (cleanup/reformat/license/merge), small commits skew corrective. So the biggest commits are exactly the least semantically meaningful per-file. | The ≥500-file skip implements this. **Caveat:** 500 is a single absolute cutoff; the MSR commit-size work (commit-size distributions, https://arxiv.org/pdf/1408.4644 , https://arxiv.org/pdf/1408.4974) shows commit sizes are heavy-tailed and **vary by repo**. A fixed 500 may be too high for small repos (lets through 200-file reformats) and occasionally too low for legitimately large features in monorepos. *Mitigation:* make threshold a **per-repo percentile** (e.g. skip commits in the top ~1–2% of that repo's file-count distribution) with 500 as an absolute fallback cap. |
| **Squash-merge / PR-squash repos** collapse a feature's many small commits into one large commit, distorting both per-file recency and the bulk threshold. | Common GitHub/GitLab workflow; not a single paper but a well-known MSR data-cleaning issue. | Detect squash/merge commits (multiple parents, or commit message patterns / `git log --merges`); optionally treat merge commits' authored-date vs commit-date, and prefer **author date** over commit date so rebases/squashes don't reset recency. |
| **Rebases / history rewrites / vendored or generated dirs** reset timestamps en masse. | — | Respect existing ignore config (`.brainignore`-style); exclude vendored/generated paths from activity computation; use author-date. |
| **Shallow clones / missing history** → no commit data → score undefined. | — | Default `recency_score=0.5` (multiplier 1.0) on missing history, so absence of data = no effect (matches the "degrade to baseline" principle). |
| **Cross-repo time skew.** "Active code of the same name across many repos" compares files from repos with different commit cadences; a slow-but-healthy repo looks uniformly dormant. | NestWeaver indexes multiple repos. | Consider computing recency **relative within each repo** (z-score or percentile of that repo's recency distribution) before applying the global multiplier, so cadence differences don't dominate cross-repo ranking. |
| **Signal ignored in practice.** | Lewis et al. 2013 (Google). | Make it explainable and bounded; expose the score in `symbol`/`context` output. |

### 12.4 Complexity / effort + reported deltas

- **Effort: Low–Medium.** Data is already partially available: `<db>.filemeta.json` tracks per-file
  mtime/size/hash, and there is `git`-based change detection in the indexer. Need: walk last N=20
  touching commits per file (`git log -n 20 --follow -- <file>` or a single `git log --name-only` pass
  bucketed per file for efficiency), parse author dates, filter bulk commits, compute the mean. One new
  sidecar (e.g. `<db>.activity.json`) or extend `filemeta`. Apply multiplier at rank time — a few lines
  in the PageRank read path.
- **Performance note:** doing `--follow` per file is O(files × history) and slow on large repos; prefer
  **one `git log --name-only --no-renames --pretty` sweep** capped at a recent window, building a
  per-file commit list in memory, then truncating to N=20 most recent per file. Recompute on `index`/`watch`.
- **Reported deltas from literature** (context, not promises): TimedPageRank's age-decay term
  *consistently improved* retrieval ranking (publication-search study). T-Rank produced rankings closer
  to human judgment. Relative churn discriminated fault-prone modules at ~**89%** (Nagappan & Ball,
  Win Server 2003). No literature reports the effect of *recency decay on code-retrieval rank quality
  specifically* — Feature 12's ranking-quality gain is **[UNVERIFIED / novel]**; validate with an
  internal A/B on "same-name disambiguation" precision@k.

---

## FEATURE 13 — affected_tests (changed files → changed symbols → reverse traversal → tests)

**Design under evaluation:** changed files → changed symbols → reverse CALLS/IMPORTS traversal
(depth 3) → test files; bucket into priority tiers (direct test of changed symbol > test of caller >
transitive). Detect test files by filename + annotation/macro heuristics. Goal: which tests an MR
should run.

This is **static, call-graph-based Regression Test Selection (RTS)** with **test prioritization**.

### 13.1 Research foundation (primary sources)

**(a) Foundational RTS framing & safety theory.**

- **Yoo & Harman, "Regression testing minimization, selection and prioritisation: a survey,"
  *Software Testing, Verification & Reliability* 22(2), 2012, pp. 67–120.**
  Wiley: https://onlinelibrary.wiley.com/doi/abs/10.1002/stvr.430 · PDF:
  http://www0.cs.ucl.ac.uk/staff/m.harman/stvr-shin-survey.pdf
  *Result / framing:* defines the three distinct problems — **minimization** (drop redundant tests),
  **selection** (pick tests relevant to a change), **prioritization** (order tests for early fault
  detection). Feature 13 is **selection + prioritization** (the tiers). Use this vocabulary in the RFC.
  Survey also catalogs selection by analysis granularity and the safety/precision tradeoff space.

- **Rothermel & Harrold, "A Safe, Efficient Regression Test Selection Technique," *ACM TOSEM* 6(2),
  1997, pp. 173–210.**
  ACM: https://dl.acm.org/doi/10.1145/248233.248262 · PDF:
  https://www.cs.purdue.edu/homes/xyzhang/fall07/Papers/p173-rothermel.pdf
  Companion: Rothermel & Harrold, "Analyzing Regression Test Selection Techniques," *IEEE TSE* 1996.
  *Result / definition:* a **safe** RTS technique, under well-defined conditions, **excludes no test
  that, if executed, would reveal a fault in the modified program.** This is the bar Feature 13 will be
  measured against — and the warning: **static call-graph RTS is generally NOT safe** unless the graph
  captures *all* dependency paths (reflection, dynamic dispatch, DI, config, data/file deps, build-time
  codegen). The class-firewall family approximates safety by being conservative; Feature 13's depth-3
  cap is an explicit *unsafe* truncation (a precision/recall trade) and must be documented as such.

**(b) Practical static RTS tools — the closest comparables to Feature 13's design.**

- **STARTS — Legunsen, Shi, Marinov, "STARTS: STAtic Regression Test Selection," ASE 2017 (tool demo).**
  PDF (slides): https://www.cs.cornell.edu/~legunsen/slides/ASE-2017.pdf · ACM:
  https://dl.acm.org/doi/10.5555/3155562.3155684 · repo: https://github.com/TestingResearchIllinois/starts
  *Result / design:* **static, class-level** RTS using the **class firewall**. Uses `jdeps` to build a
  class-dependency graph, then **reverse-traverses to find tests transitively reachable from changed
  classes** — structurally identical to Feature 13's "reverse CALLS/IMPORTS traversal to test files."
  *Caveat (reflection):* static RTS **cannot see reflective/dynamically-loaded dependencies**, so it can
  miss tests (unsafe) unless conservatively over-approximated. (See Shi et al., "Reflection-Aware Static
  RTS," OOPSLA 2019, https://mir.cs.illinois.edu/marinov/publications/ShiETAL19ReflectionAwareRTS.pdf —
  explicitly motivated by STARTS being unsafe under reflection.)

- **Ekstazi — Gligoric, Eloussi, Marinov, "Practical Regression Test Selection with Dynamic File
  Dependencies," ISSTA 2015; tool: "Ekstazi: Lightweight Test Selection," ICSE 2015 demo.**
  Tool PDF: https://users.ece.utexas.edu/~gligoric/papers/GligoricETAL15EkstaziTool.pdf · repo:
  https://github.com/gliga/ekstazi
  *Result:* **dynamic, file-level** RTS — instruments test runs to record which files each test
  *actually* touches; selects tests whose recorded dependency set intersects the changed files.
  Reported (per the tool/ISSTA work and follow-ups): reduced **end-to-end testing time ~32% on average,
  and ~54% for longer-running suites** vs. running all tests. Being dynamic, it captures reflection/DI
  that static analysis misses — the key axis on which Feature 13 (static) is weaker.
  - **EkstaziSharp (.NET) — Vasic et al., "File-Level vs. Module-Level Regression Test Selection for
    .NET," ESEC/FSE 2017:** https://par.nsf.gov/servlets/purl/10055459 — directly studies the
    **granularity tradeoff** Feature 13 faces (file vs finer-grained selection).

- **Microsoft Test Impact Analysis (TIA)** — productized dynamic RTS in Azure DevOps / VSTest, mapping
  tests↔code via runtime coverage to select tests per change. (Industrial analog; dynamic, coverage-based.)

**(c) Industrial-scale RTS / predictive selection — what large monorepos actually do.**

- **Memon, Gao, Nguyen, Dhanda, Nickell, Siemborski, Micco, "Taming Google-Scale Continuous Testing,"
  ICSE-SEIP 2017.**
  Google PDF: https://research.google.com/pubs/archive/45861.pdf · ACM:
  https://dl.acm.org/doi/10.1109/ICSE-SEIP.2017.16
  *Result:* Google cannot run every test on every change; uses **dependency-graph-based RTS** (build
  graph: which tests transitively depend on changed targets) plus result-history analysis to control
  test workload "without compromising quality." Validates the *reverse-reachability* core of Feature 13
  at scale, but anchored on the **build dependency graph** (which is precise) rather than a parsed
  source call graph (which is approximate).

- **Machalica, Samylkin, Porth, Chandra, "Predictive Test Selection," ICSE-SEIP 2019 (Facebook/Meta).**
  arXiv: https://arxiv.org/abs/1810.05286 · ACM: https://dl.acm.org/doi/10.1109/ICSE-SEIP.2019.00018
  *Result:* ML model selects tests per change; **reduces total testing infrastructure cost by ~2×**
  while still reporting **>95% of individual test failures and >99.9% of faulty changes.** Features
  include change↔test relationships in the dependency graph (distance), file/extension metadata, and
  historical test failure/flakiness rates. *Implication:* the state of the art **augments graph
  reachability with historical signals and accepts a measured recall (<100%) rather than chasing
  provable safety.** Feature 13 can adopt the same posture: graph-based candidate set + (later)
  history-based ranking, with an explicit recall target.

### 13.2 Recommended approach for NestWeaver (grounded)

1. **Position it honestly as static, source-graph RTS + prioritization** (Yoo & Harman vocabulary).
   It is **not** "safe" in the Rothermel–Harrold sense and the RFC must say so. It is closest to
   **STARTS** (static reverse-reachability) but on a *source-parsed* call graph rather than bytecode
   `jdeps`, so it is *less* sound than STARTS (NestWeaver's CALLS/IMPORTS edges carry confidence scores
   and miss dynamic dispatch/reflection/macros).

2. **Reverse-reachability is the right mechanism** — it is exactly what STARTS and Google TAP do.
   Keep changed-files → changed-symbols → reverse CALLS/IMPORTS → test files.

3. **Tiering = prioritization (Yoo & Harman).** The proposed tiers map cleanly to "distance in the
   dependency graph," the same feature Meta's model weighs most:
   - **Tier 1 (direct):** test directly references/CALLS the changed symbol (graph distance 1).
   - **Tier 2 (caller):** test covers a direct caller of the changed symbol (distance 2).
   - **Tier 3 (transitive):** reachable at distance 3.
   Use **edge confidence** (NestWeaver already weights CALLS=1.0, IMPORTS=0.8, USES=0.5, ACCESSES=0.4)
   to order within a tier — high-confidence call paths first. Cap at depth 3 as specified, but **report
   the cap** (precision/recall trade).

4. **Test-file detection — combine filename + annotation/macro heuristics across languages.** Examples
   to encode (NestWeaver indexes 32 languages): Rust `#[test]` / `#[cfg(test)]` / `mod tests`;
   Python `test_*.py` / `*_test.py` / `pytest` / `unittest.TestCase`; JS/TS `*.test.*` / `*.spec.*` /
   `describe`/`it`; Go `*_test.go` + `func TestXxx(*testing.T)`; JUnit `@Test`; Java `*Test.java`. The
   *annotation/macro* layer matters because filename conventions are inconsistent — this mirrors STARTS
   needing to identify test classes structurally.

5. **Set an explicit recall target, measured against "run-all," like Meta did** (their bar: >95%
   failure recall). Don't claim safety; claim measured recall and report it.

### 13.3 Pitfalls / failure modes + mitigations

| Pitfall | Evidence / reasoning | Mitigation |
|---|---|---|
| **Static RTS is unsafe vs dynamic** — misses reflection, dynamic dispatch, DI, mocking, runtime config, data-driven tests, build-time codegen, FFI. Will **drop tests that would have failed.** | Rothermel & Harrold (safety definition); Ekstazi (dynamic) vs STARTS (static) is precisely this axis; Shi et al. OOPSLA 2019 created reflection-aware RTS *because* STARTS was unsafe under reflection. | Document as best-effort, not safe. Offer a **conservative mode**: when a changed file has low-confidence/edge coverage, fall back to selecting a broader set or the whole module. Combine with a periodic full run (Google/Meta both keep a full safety net). |
| **Depth-3 truncation drops far-but-real tests.** | The spec's own cap; RTS theory says safety needs *all* reachable tests. | Make depth configurable; emit a "tests beyond depth 3 exist" warning count so users know the candidate set was truncated. |
| **Cross-file/cross-language edges missing** (e.g. a JS test exercising a Rust binding, or test fixtures loaded as data). | Source-graph parsers don't link across language/process boundaries. | Add a "co-change" fallback tier from git history (files historically changed together with the test) — a cheap precision/recall booster aligned with Google's use of history. |
| **Flaky tests pollute the result** — selecting them adds noise; their failures aren't change-related. | Machalica et al. explicitly model flakiness. | Track per-test historical failure/flake rate (NestWeaver already has `<db>.interactions.json` infra for usage signals); deprioritize or flag flaky tests. |
| **Squash-merge / large MRs** → "changed files" set is huge, traversal explodes, selection ≈ run-all. | Same monorepo workflow issue as Feature 12. | Cap fan-out; if selection exceeds X% of all tests, recommend run-all (selection gives no benefit past a break-even point — a known RTS result). |
| **No tests found ≠ safe to skip.** Absence of a reverse path may mean missing edges, not absence of coverage. | Static-graph incompleteness. | Distinguish "no tests selected (high confidence)" from "no path found (low coverage of this file)"; in the latter case warn rather than greenlight. |
| **Adoption/trust.** A selector that ever misses a real failure loses developer trust fast. | Lewis et al. 2013 (signals get ignored when not trusted). | Be transparent about recall; show *why* each test was selected (the path/tier); pair with full-run CI gate initially. |

### 13.4 Complexity / effort + reported deltas

- **Effort: Medium.** Mechanisms largely exist: NestWeaver has the CALLS/IMPORTS graph with edge
  confidence, multi-language symbol parsing, and `impact` already does forward/reverse traversal with a
  `--depth` flag (reuse it — `impact` is the inverse direction of what's needed). New work: (1) map
  changed files → changed symbols (diff vs. parsed symbols — `git diff --name-only` + symbol ranges);
  (2) reverse traversal terminating at test nodes; (3) test-node detection heuristics per language;
  (4) tier bucketing by graph distance + edge confidence. One command (`nestweaver affected-tests`) and/or
  an MCP tool.
- **Reported deltas from comparable tools** (context, not promises for a static source-graph variant):
  - **Ekstazi** (dynamic, file-level): ~**32%** average end-to-end test-time reduction, ~**54%** on
    longer suites. (https://users.ece.utexas.edu/~gligoric/papers/GligoricETAL15EkstaziTool.pdf)
  - **STARTS** (static, class firewall): comparable selection to Ekstazi but **less precise / more
    conservative** (selects more tests) because static analysis over-approximates; its appeal is needing
    only compile-time info, no instrumentation — the same appeal as Feature 13. Exact STARTS reduction
    percentages **[UNVERIFIED]** from the retrieved demo slides (compressed PDF); cite the ASE 2017 demo
    and OOPSLA 2019 follow-up for the safety discussion rather than a specific % here.
  - **Meta Predictive Test Selection:** **~2× cost reduction** at **>95%** individual-test-failure recall,
    **>99.9%** faulty-change recall (https://arxiv.org/abs/1810.05286). This is the realistic ceiling and
    the recall bar to aim for; Feature 13's pure-static v1 will likely have *lower* recall and should be
    validated against run-all before being allowed to gate anything.
  - **Google TAP:** dependency-graph RTS at scale, framed as workload control "without compromising
    quality" (https://research.google.com/pubs/archive/45861.pdf) — validates the reverse-reachability
    core but on a precise build graph.

---

## Cross-cutting notes

- Both features lean on the same **git-history mining substrate** (commit walk, author-date,
  bulk/squash-commit detection, per-file co-change). Build it once and share it. The **MSR commit-size
  literature** (Hindle/German/Holt MSR 2008; commit-size-distribution papers
  https://arxiv.org/pdf/1408.4644 , https://arxiv.org/pdf/1408.4974) supports per-repo, percentile-based
  bulk-commit detection over a single global file-count cutoff.
- **Established vs speculative:** *Established* — exponential age-decay improves temporal ranking
  (TimedPageRank/T-Rank); relative churn predicts defects (Nagappan & Ball, Hassan); reverse-reachability
  RTS works and saves time (Ekstazi/STARTS/Google/Meta); static RTS is unsafe vs dynamic
  (Rothermel–Harrold + Ekstazi-vs-STARTS). *Speculative / unverified* — that recency-decay specifically
  improves *code-retrieval rank quality* (Feature 12 gain is novel, [UNVERIFIED], needs A/B); exact
  STARTS reduction %; exact Hindle large-commit file-count threshold (top-decile/perfective-skew is
  supported; the precise numeric cutoff is **[UNVERIFIED]** from retrieved material).

## Source index

- Temporal PageRank — Rozenshtein & Gionis, ECML-PKDD 2016: https://link.springer.com/chapter/10.1007/978-3-319-46227-1_42
- T-Rank / temporal web ranking survey: https://informationr.net/ir/18-3/paper586.html
- Time-weighted web authoritative ranking: https://link.springer.com/article/10.1007/s10791-010-9138-4
- TimedPageRank case study (WI 2005): https://www.cs.uic.edu/~liub/publications/WI-05-temp.pdf
- Nagappan & Ball, ICSE 2005 (relative churn): https://www.microsoft.com/en-us/research/publication/use-of-relative-code-churn-measures-to-predict-system-defect-density/
- Hassan, ICSE 2009 (change entropy): https://dl.acm.org/doi/10.1109/ICSE.2009.5070510
- Lewis et al., ICSE 2013 (bug prediction adoption at Google): https://research.google/pubs/does-bug-prediction-support-human-developers-findings-from-a-google-case-study/
- Yoo & Harman survey, STVR 2012: https://onlinelibrary.wiley.com/doi/abs/10.1002/stvr.430
- Rothermel & Harrold, safe RTS, TOSEM 1997: https://dl.acm.org/doi/10.1145/248233.248262
- STARTS, ASE 2017: https://github.com/TestingResearchIllinois/starts , https://www.cs.cornell.edu/~legunsen/slides/ASE-2017.pdf
- Reflection-aware static RTS, OOPSLA 2019: https://mir.cs.illinois.edu/marinov/publications/ShiETAL19ReflectionAwareRTS.pdf
- Ekstazi, ICSE/ISSTA 2015: https://github.com/gliga/ekstazi , https://users.ece.utexas.edu/~gligoric/papers/GligoricETAL15EkstaziTool.pdf
- EkstaziSharp file vs module .NET, ESEC/FSE 2017: https://par.nsf.gov/servlets/purl/10055459
- Google TAP, ICSE-SEIP 2017: https://research.google.com/pubs/archive/45861.pdf
- Meta Predictive Test Selection, ICSE-SEIP 2019: https://arxiv.org/abs/1810.05286
- Hindle/German/Holt, large commits, MSR 2008: https://dl.acm.org/doi/10.1145/1370750.1370773
- Commit-size distribution: https://arxiv.org/pdf/1408.4644 , https://arxiv.org/pdf/1408.4974
