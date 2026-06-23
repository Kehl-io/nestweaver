#!/usr/bin/env python3
"""
token_savings.py — measure token savings from NestWeaver vs raw source files.

Usage:
    python3 benchmarks/token_savings.py --results results.json --output benchmarks/token-savings.json

Auto-installs tiktoken if missing.
"""

import argparse
import importlib
import json
import subprocess
import sys
import time
from pathlib import Path


def ensure_tiktoken() -> None:
    """Install tiktoken if it is not already available."""
    try:
        importlib.import_module("tiktoken")
    except ModuleNotFoundError:
        print("tiktoken not found — installing...", flush=True)
        subprocess.check_call(
            [sys.executable, "-m", "pip", "install", "--quiet", "tiktoken"]
        )
        # Reload sys.path so the newly installed package is importable.
        importlib.invalidate_caches()


def count_tokens(text: str, encoder) -> int:
    return len(encoder.encode(text))


def nestweaver_query(query: str, repo: str, kind: str = "context") -> str:
    """
    Run a nestweaver CLI query and return the raw stdout.

    kind: "context" for NL queries, "search" for exact-symbol queries.
    """
    cmd = ["nestweaver", kind, query, "--repo", repo, "--json"]
    try:
        result = subprocess.run(
            cmd, capture_output=True, text=True, timeout=60
        )
        return result.stdout
    except subprocess.TimeoutExpired:
        return ""
    except FileNotFoundError:
        raise RuntimeError(
            "nestweaver not found on PATH. "
            "Install NestWeaver and ensure `nestweaver` is executable."
        )


def measure_savings(
    results_path: Path,
    output_path: Path,
    encoder,
) -> list[dict]:
    with results_path.open() as f:
        results = json.load(f)

    savings = []

    for entry in results:
        repo = entry.get("repo", "")
        query = entry.get("query", "")
        query_kind = entry.get("kind", "nl")  # "nl" or "exact"
        raw_files: list[str] = entry.get("raw_file_contents", [])

        # Count tokens in raw files.
        raw_tokens = sum(count_tokens(content, encoder) for content in raw_files)

        # Run nestweaver and count response tokens.
        nw_kind = "context" if query_kind == "nl" else "search"
        t0 = time.monotonic()
        nw_response = nestweaver_query(query, repo, kind=nw_kind)
        latency_ms = (time.monotonic() - t0) * 1000

        response_tokens = count_tokens(nw_response, encoder)

        if raw_tokens > 0:
            savings_pct = (1 - response_tokens / raw_tokens) * 100
        else:
            savings_pct = 0.0

        record = {
            "repo": repo,
            "query": query,
            "kind": query_kind,
            "raw_tokens": raw_tokens,
            "response_tokens": response_tokens,
            "token_savings_pct": round(savings_pct, 2),
            "latency_ms": round(latency_ms, 1),
        }
        savings.append(record)

        print(
            f"  [{repo}] {query!r:40s} "
            f"raw={raw_tokens:>7,}  nw={response_tokens:>6,}  "
            f"savings={savings_pct:5.1f}%  latency={latency_ms:.0f}ms"
        )

    return savings


def summarise(savings: list[dict]) -> dict:
    if not savings:
        return {}

    total_raw = sum(r["raw_tokens"] for r in savings)
    total_nw = sum(r["response_tokens"] for r in savings)
    overall_savings = (1 - total_nw / total_raw) * 100 if total_raw else 0.0

    by_repo: dict[str, dict] = {}
    for r in savings:
        repo = r["repo"]
        by_repo.setdefault(repo, {"raw": 0, "nw": 0, "count": 0})
        by_repo[repo]["raw"] += r["raw_tokens"]
        by_repo[repo]["nw"] += r["response_tokens"]
        by_repo[repo]["count"] += 1

    repo_summary = {
        repo: {
            "raw_tokens": v["raw"],
            "response_tokens": v["nw"],
            "token_savings_pct": round(
                (1 - v["nw"] / v["raw"]) * 100 if v["raw"] else 0.0, 2
            ),
            "query_count": v["count"],
        }
        for repo, v in by_repo.items()
    }

    return {
        "overall": {
            "total_raw_tokens": total_raw,
            "total_response_tokens": total_nw,
            "token_savings_pct": round(overall_savings, 2),
            "query_count": len(savings),
        },
        "by_repo": repo_summary,
    }


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--results",
        required=True,
        type=Path,
        help="Path to benchmark results JSON (produced by run_benchmarks.py).",
    )
    parser.add_argument(
        "--output",
        default=Path("benchmarks/token-savings.json"),
        type=Path,
        help="Where to write the token-savings output JSON.",
    )
    args = parser.parse_args()

    if not args.results.exists():
        sys.exit(f"Results file not found: {args.results}")

    ensure_tiktoken()

    import tiktoken  # noqa: PLC0415 — imported after ensure_tiktoken()

    encoder = tiktoken.get_encoding("cl100k_base")

    print(f"Measuring token savings from {args.results} …")
    savings = measure_savings(args.results, args.output, encoder)
    summary = summarise(savings)

    output = {"summary": summary, "per_query": savings}

    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(output, indent=2))
    print(f"\nWrote {args.output}")
    print(
        f"Overall savings: {summary.get('overall', {}).get('token_savings_pct', 'n/a')}% "
        f"across {summary.get('overall', {}).get('query_count', 0)} queries"
    )


if __name__ == "__main__":
    main()
