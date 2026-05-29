# Retrieval-quality eval harness (P0.3)

`nestweaver eval` scores retrieval quality (nDCG@10 / MRR / precision@5) over a
**judged** query set. It exists so the off-by-default quality features
(F6 ranking priors, F7 pseudo-relevance feedback, F1 interaction feedback,
F12 CodeRank, F17 reranking) can be **measured** before being trusted, instead
of shipped on a hunch.

## ⚠️ Read this before trusting any number

- **You need REAL human relevance labels over the corpus you actually index.**
  A judged query is `(query, {node-uid → graded relevance 0..=3})` where the
  grades come from a human (or a validated proxy) looking at *your* code/notes.
- **`eval-queries.example.jsonl` is a FORMAT TEMPLATE, not a benchmark.** Its
  UIDs are placeholders that will not exist in your database. Pointing the
  harness at it tells you the file parses — nothing about retrieval quality.
- **Metrics on a tiny or synthetic set are not authoritative.** A 3–4 query set
  swings wildly; one query flipping rank dominates the mean.
- **Do not trust a small mean delta.** Before believing a feature helps, look at
  the *per-query* win/loss counts (`eval compare` prints them) and use
  time-based or query-based train/test splits so you are not tuning on the same
  queries you evaluate on. The project's `>= 5% nDCG@10` gate is a floor on the
  mean, not a substitute for that scrutiny.

## File format

JSON array **or** JSONL (one object per line). Each object:

| field       | type                  | notes                                              |
|-------------|-----------------------|----------------------------------------------------|
| `query`     | string (required)     | the seed string fed into hybrid retrieval          |
| `intent`    | string (optional)     | `find-definition` / `understand-architecture` / `analyze-impact` / `general-context` / `project-context` (lenient aliases accepted) |
| `relevance` | map<uid, 0..=3>       | graded relevance per node UID (0 = irrelevant, 3 = ideal) |

UIDs are NestWeaver node UIDs (e.g. `sym:...`, `note:...`, `sec:...`). Find them
with `nestweaver symbol "<name>" --json` or `nestweaver search "<term>" --json`.

## Building a real judged set

1. Index your repo / vault: `nestweaver index` (and `nestweaver brain add ...`).
2. Pick queries representative of how the corpus is actually searched.
3. For each query, run retrieval, inspect the top candidates, and assign each a
   grade 0..=3 by hand. Record the node UIDs (`--json` gives them).
4. Save as JSONL, one judged query per line.

## Running

```sh
# Score a judged set once.
nestweaver eval run --queries ./my-judged.jsonl
nestweaver eval run --queries ./my-judged.jsonl --json          # full EvalReport
nestweaver eval run --queries ./my-judged.jsonl --prf --rerank  # features ON

# Baseline vs a toggled feature on the SAME set (judge the >= 5% nDCG@10 gate).
nestweaver eval compare --queries ./my-judged.jsonl --prf
nestweaver eval compare --queries ./my-judged.jsonl --rerank
nestweaver eval compare --queries ./my-judged.jsonl --prf --rerank --json
```

`eval compare` requires at least one of `--prf` / `--rerank` (the feature to
toggle ON in the treatment run; the baseline always has it OFF). It prints the
mean nDCG@10 delta, relative change, and per-query win/loss/tie counts.
