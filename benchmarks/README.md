# NestWeaver Benchmark Suite

Measures query latency, answer quality, and token savings across large, real-world repositories.

## Quick Start

```bash
# Run the full benchmark
python3 benchmarks/run_benchmarks.py --queries benchmarks/queries.json --output results.json

# Measure token savings
python3 benchmarks/token_savings.py --results results.json --output benchmarks/token-savings.json
```

## Metrics

| Metric | Description | Unit |
|--------|-------------|------|
| `latency_ms` | Wall-clock time from query submission to first byte of response | milliseconds |
| `total_latency_ms` | Wall-clock time from query submission to last byte of response | milliseconds |
| `tokens_in_response` | Tokens in the NestWeaver response (cl100k_base encoding) | tokens |
| `tokens_in_raw_files` | Tokens across all raw source files that would answer the query | tokens |
| `token_savings_pct` | `(1 - tokens_in_response / tokens_in_raw_files) * 100` | percent |
| `recall_at_1` | Whether the top-1 result matches the ground-truth symbol | 0 or 1 |
| `recall_at_5` | Whether any of the top-5 results match the ground-truth symbol | 0 or 1 |

## Repositories

| Repo | Language | LOC (approx) | Notes |
|------|----------|---------------|-------|
| linux | C | ~28 M | Kernel — extremely large, C macros everywhere |
| kubernetes | Go | ~3 M | Cloud orchestration, heavy interface use |
| react | JavaScript/TypeScript | ~200 K | UI framework, well-structured packages |
| rust | Rust | ~1 M | Self-hosting compiler, complex type system |
| nextjs | TypeScript | ~300 K | Full-stack framework, mixed SSR/client code |

## Requirements

- Python 3.9+
- `tiktoken` (auto-installed by `token_savings.py` if missing)
- NestWeaver CLI (`nestweaver`) on `$PATH`
- Each repo indexed: `nestweaver brain add-source <path> --db $NESTWEAVER_DB`

## Isolation

Each benchmark run is isolated:

- Queries are executed sequentially to avoid cache interference between runs.
- The NestWeaver DB is not modified during a run (read-only queries).
- OS file-system cache is **not** flushed between queries — results reflect warm-cache performance, which matches typical developer workflows.
- To measure cold-cache performance, run `sync && sudo sh -c 'echo 3 > /proc/sys/vm/drop_caches'` (Linux) before each query.

## Methodology

### Token savings

For each query we compare:

1. **Raw file tokens** — the total token count of every source file that a developer would need to read to answer the query manually. We identify these files by asking a grader LLM which files are ground-truth relevant, then count all their tokens with tiktoken (`cl100k_base`).
2. **NestWeaver response tokens** — the token count of the actual response returned by `nestweaver context` or `nestweaver search`.

Token savings percentage: `(1 - response_tokens / raw_tokens) * 100`.

### Recall

Ground-truth symbols for the exact-symbol queries are hand-labelled in `queries.json` (the `exact_queries` lists). Recall is computed by checking whether the labelled symbol appears in the top-k results returned by NestWeaver.

NL query recall is evaluated separately using an LLM judge that scores relevance of each returned result on a 0–2 scale (0 = irrelevant, 1 = partially relevant, 2 = fully relevant). Mean relevance score is reported as `nl_relevance_mean`.
