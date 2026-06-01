## Summary

This branch lands two large bodies of work:

1. **Next-gen web UI** — a Three.js / React-Three-Fiber graph renderer replacing the previous Sigma.js view, plus the supporting `nestweaver-algorithms` (pure-compute, WASM-compatible) and `nestweaver-wasm` crates so graph algorithms (PPR, impact BFS) can run client-side.
2. **v0.9.1 retrieval quality & feedback** — a set of code+notes intelligence features, two bug fixes, and an offline evaluation harness so the new ranking features can be measured rather than trusted on a hunch.

Everything is gated behind the existing CLI/MCP surface; the new quality features are **off by default** and opt-in.

## Web UI / rendering

- GPU-accelerated R3F graph renderer with animated force settling, always-visible labels, click-to-focus, node drag, edge gradients, and semantic-zoom camera bridge.
- 3D community hulls / cluster overlay and impact ripple on selection.
- WASM engine wired end-to-end (`?engine=wasm`) so PPR/impact run in the browser; SSE live-update stream (`graph:updated`, `pagerank:recomputed`, `full_refresh`).
- New web API: `GET /api/v1/version`, `GET /api/v1/snapshot.msgpack` (with `X-Graph-Generation` header), `GET /api/v1/events`.

## Retrieval quality & intelligence features

- Per-path dampen/boost ranking priors; git-activity-dampened CodeRank; lightweight result reranker (all opt-in).
- BM25 pseudo-relevance feedback (PRF) for query expansion.
- Trigram-accelerated regex search + pattern counting.
- `read_symbols` source-window reads and inline high-confidence result bodies (uses the new `Symbol.end_line` span).
- Document-graph tooling over the notes vault (`brain.*`), API-contract graph (Contract nodes + IMPLEMENTS edges + drift), memory-bank semantics (typed edges, lint, consolidate, related), and an `investigate` context-bundle primitive.
- `affected_tests` — static regression-test selection for PR test scoping (reaches Jest/Vitest tests).
- Agent feedback loop (interaction memory: success signal + `interactions show`) and agent-guidance generation.

## Bug fixes

- `project_context` now surfaces project member notes that were being dropped.
- `--config` is honored on `brain` read commands when resolving the database path.
- `--force` re-index is now idempotent (no duplicate-PK crash); `broken-links` surfaces unresolved wikilinks; regex line numbers, install-hook dry-run delta, and multi-handler coverage corrected.

## Evaluation harness (new)

`nestweaver eval run` / `eval compare` score retrieval quality (nDCG@10 / MRR / precision@5) over a **judged** query set, reusing the shipped hybrid-retrieval path so it measures exactly what the product serves. `eval compare --prf/--rerank` runs baseline-vs-treatment and applies a `>=5%` nDCG@10 gate.

> The metrics are only meaningful with real human relevance labels over the corpus you actually index. The bundled `examples/eval/eval-queries.example.jsonl` is a **format template**, not a benchmark — the help text and `examples/eval/README.md` say so explicitly, and the gate caveats (per-query win/loss, train/test splits) are surfaced in the tool output.

## Persistence / infra

- `Symbol.end_line` added to the schema (full source span).
- `graph_generation` persisted across restarts; ZSTD-compressed response cache for the web API.

## Testing

- `cargo test --workspace` → **1094 passed, 0 failed**
- `cargo clippy --workspace --all-targets --all-features -- -D warnings` → clean
- `cargo fmt --all -- --check` → clean

New features were developed test-first and manually exercised end-to-end; a QA sweep over both existing and new functionality drove the fix commits above.
