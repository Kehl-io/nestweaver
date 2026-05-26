# NestWeaver `context` Command — Design Spec

**Date:** 2026-05-22
**Status:** Approved

---

## Overview

The `context` command extracts a task-focused subgraph from the knowledge graph
using Personalized PageRank. Given seed inputs (symbol names, UIDs, or file
paths), it returns only the structurally relevant symbols ranked by relevance.

Pure graph math — no AI, no network calls, no token budgeting. The agent
controls how much it reads.

---

## Interface

```
nestweaver context [OPTIONS] [SEEDS]...

Arguments:
  [SEEDS]...  Symbol names, UIDs, or file paths (not required when --feature is set)

Options:
  --feature <NAME>    Resolve a declared feature bundle from the instance config
  --config <PATH>     Path to instance config file (required with --feature)
  --json              Output as JSON
  --db <DB>           Path to the database file [default: ./nestweaver.lbug]
```

Input auto-detection (for positional seeds):
- Starts with `sym:` or `repo:` → UID (direct graph lookup)
- Contains `/` or has a source file extension (`.js`, `.ts`, `.py`, `.java`,
  `.go`) → file path (seed from all symbols in that file)
- Otherwise → symbol name (BM25 search, use top matches as seeds)

When `--feature <name>` is given, the feature's `entry_points` are resolved as
seeds across all repos listed in the feature bundle. See the
[Cross-Repo Features design](cross-repo-features.md) for the feature output format.

---

## Algorithm

### 1. Parse and resolve seeds

For each input argument:
- **UID:** `store.lookup_symbol(uid)` → single node
- **File path:** `store.symbols_in_file(path)` → all symbols in that file
- **Name:** `store.search_symbols_by_name(name, 5)` → top 5 matches

Collect all resolved symbol UIDs into a seed set. If no seeds resolve,
return an error with a suggestion to use `nestweaver search`.

### 2. Load adjacency data

Load all Symbol UIDs and all directed edges (CALLS, IMPORTS, EXTENDS_SYM,
IMPLEMENTS_SYM, MEMBER_OF) into an in-memory adjacency list. Both forward
and reverse edges are needed (PPR propagates relevance in both directions
through the call graph).

This is the same data `compute_pagerank` already loads, with the addition
of reverse edges.

### 3. Run Personalized PageRank

Standard PPR with personalization vector:

```
personalization[v] = 1/|seeds|  if v is a seed node
personalization[v] = 0          otherwise

Initialize: scores[v] = personalization[v] for all v

Iterate (up to 20 times or until convergence < 1e-6):
  new_scores[v] = (1 - d) * personalization[v]
                + d * sum(scores[u] / out_degree[u]) for each u that links to v

  scores = new_scores
```

Damping factor `d = 0.85`. Convergence = max absolute change across all nodes.

### 4. Rank and filter

Sort all nodes by PPR score descending. Filter out:
- Nodes with score below `min_score` threshold (1e-4 by default)
- Seed nodes are always included regardless of score

### 5. Enrich and format output

For each result node, include:
- UID, name, kind, file_path, start_line, signature
- PPR relevance score
- Which seed it's most connected to (optional, for agent understanding)

Also query:
- Cross-repo links from any result node
- Source availability: check which file paths exist on disk in the
  configured workspace

---

## Output Format

### Default (structured text)

```
Seeds (2 resolved):
  processPayment  Function  src/checkout/payment.ts:42
  CheckoutService  Class  src/checkout/service.ts:8

Connected (12 symbols, ranked by relevance):
  validateCard      Function   src/checkout/validation.ts:15   0.82
  PaymentGateway    Interface  src/gateway/types.ts:3          0.71
  StripeGateway     Class      src/gateway/stripe.ts:12        0.64
  formatAmount      Function   src/utils/currency.ts:28        0.41
  ...

Cross-repo links:
  @myorg/payment-sdk  SharedImport  0.90

Source availability:
  Local: src/checkout/ src/utils/
  Remote: src/gateway/ (not pulled)
```

### JSON (`--json`)

```json
{
  "seeds": [
    {"uid": "sym:...", "name": "processPayment", "kind": "Function", "file_path": "src/checkout/payment.ts", "start_line": 42}
  ],
  "connected": [
    {"uid": "sym:...", "name": "validateCard", "kind": "Function", "file_path": "src/checkout/validation.ts", "start_line": 15, "signature": "function validateCard(card)", "relevance": 0.82}
  ],
  "cross_repo_links": [
    {"package": "@myorg/payment-sdk", "link_type": "SharedImport", "confidence": 0.90}
  ],
  "source_availability": {
    "local": ["src/checkout/", "src/utils/"],
    "remote": ["src/gateway/"]
  }
}
```

---

## Error Handling

| Condition | Message | Exit |
|-----------|---------|------|
| No seeds resolve | "No matching symbols found. Try `nestweaver search <term>` to find symbols." | 2 |
| Ambiguous name (multiple matches) | List candidates with UIDs | 3 |
| Empty graph | "No symbols indexed. Run `nestweaver index --repo <path>` first." | 1 |
| Database not found | "Database not found at <path>. Run `nestweaver index` first." | 1 |

---

## Implementation Location

| Component | File | What to add |
|-----------|------|-------------|
| PPR algorithm | `nestweaver-store/src/ranking.rs` | `personalized_pagerank(seeds, damping, iterations) -> Vec<(uid, score)>` |
| File symbol lookup | `nestweaver-store/src/read.rs` | `symbols_in_file(path) -> Vec<Symbol>` |
| Context query | `nestweaver-engine/src/query.rs` | `build_context(store, seeds) -> ContextResult` |
| CLI command | `src/main.rs` | `Context` variant in Commands enum |

No new crates. No new dependencies.

---

## Exit Codes

Same as existing commands: 0 success, 1 error, 2 not found, 3 ambiguous.
