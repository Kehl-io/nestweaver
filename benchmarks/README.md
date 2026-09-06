# NestWeaver Benchmark Suite

Competitive benchmark comparing NestWeaver against Graphify on indexing speed, query latency, and token savings.

## Quick Start

```bash
# Full run (3 iterations per measurement, ~30-60 min depending on repos)
benchmarks/run.sh

# Quick single-iteration run
NUM_RUNS=1 benchmarks/run.sh
```

## Isolation

The suite is fully self-contained — nothing touches your global NestWeaver install:

- **NestWeaver** is built from source into `/private/tmp/nestweaver-bench/local/` with the `embed` feature (`run.sh` additionally enables `metal` when it detects Apple Silicon)
- **A dedicated daemon** starts per-repo with its own DB socket (your production daemon is untouched)
- **Graphify** is installed into a Python virtual environment under the bench root
- **Python deps** (matplotlib, tiktoken) go into a venv at `venvs/bench/`
- **All indexes, results, and reports** live under `/private/tmp/nestweaver-bench/`

To clean up everything: `rm -rf /private/tmp/nestweaver-bench/`

## What It Measures

| Metric | NestWeaver command | Description |
|--------|-------------------|-------------|
| Fresh indexing | `nestweaver index` | Time to parse + build knowledge graph from scratch |
| NL query latency | `nestweaver search` | Text/semantic search response time |
| Exact query latency | `nestweaver context` | Structural graph traversal from seed symbols |
| Token savings | `token_savings.py` | NestWeaver response tokens vs raw source file tokens |

## Repositories

| Repo | Language | Size | Notes |
|------|----------|------|-------|
| Tailwind CSS | TS/JS | Small | Utility CSS framework |
| Deno | Rust | Medium (~5.7K files) | JavaScript/TypeScript runtime |
| Next.js | TypeScript | Large (~29K files) | Full-stack React framework |
| Elasticsearch | Java | Huge (~25K+ files) | Distributed search engine |

## Output

Results land in `/private/tmp/nestweaver-bench/`:

```
results/
  metadata.json               # Hardware, versions, repo SHAs
  <repo>-nestweaver.json      # Per-repo NestWeaver results
  <repo>-graphify.json        # Per-repo competitor results
  token-savings.json           # Token comparison data
report/
  benchmark-report.md          # Markdown report with tables
  indexing-speed.svg           # Bar charts
  query-latency.svg
```

## Configuration

- **`queries.json`** — repos to benchmark and queries to run (NL + exact per repo)
- **`NUM_RUNS`** env var — iterations per measurement (default: 3)
- **`BENCH_ROOT`** env var — override the working directory (default: `/private/tmp/nestweaver-bench`)

## Files

| File | Purpose |
|------|---------|
| `run.sh` | Orchestrator: clone repos, build tools, run benchmarks, generate report |
| `measure.sh` | Measurement functions sourced by run.sh |
| `charts.py` | SVG chart generation + markdown report |
| `token_savings.py` | Token count comparison (NestWeaver vs raw files) |
| `queries.json` | Query definitions per repo |

## Methodology

### Token savings

For each query we compare:

1. **Raw file tokens** — total token count of source files in the repo (sampled, cl100k_base encoding)
2. **NestWeaver response tokens** — token count of the response from `nestweaver context` or `nestweaver search`

Token savings: `(1 - response_tokens / raw_tokens) * 100`

### Query types

- **NL queries** ("process scheduler", "hooks implementation") → routed to `nestweaver search` for text/semantic matching
- **Exact queries** ("createElement", "Schedule") → routed to `nestweaver context` for structural graph traversal from seed symbols
