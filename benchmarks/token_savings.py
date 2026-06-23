#!/usr/bin/env python3
"""
token_savings.py — measure token savings from NestWeaver vs raw source files.

For each query, compares the NestWeaver response tokens against the raw source
of the files that NestWeaver actually references.  This shows the real value:
"you'd need to read these 12 files (47K tokens) but NestWeaver summarises the
answer in 2.3K tokens."

Usage:
    python3 benchmarks/token_savings.py \
        --queries benchmarks/queries.json \
        --index-dir /tmp/bench/indexes \
        --repos-dir /tmp/bench/repos \
        --output results/token-savings.json

Auto-installs tiktoken if missing.
"""

import argparse
import importlib
import json
import os
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
        importlib.invalidate_caches()


def count_tokens(text: str, encoder) -> int:
    return len(encoder.encode(text))


def nestweaver_query(query: str, db: str, kind: str, nw_bin: str) -> str:
    """
    Run a nestweaver CLI query and return the raw stdout.

    kind: "search" for NL queries, "context" for exact/symbol queries.
    """
    cmd = [nw_bin, kind, "--db", db, "--json", query]
    try:
        result = subprocess.run(
            cmd, capture_output=True, text=True, timeout=60
        )
        return result.stdout
    except subprocess.TimeoutExpired:
        return ""
    except FileNotFoundError:
        raise RuntimeError(
            f"nestweaver not found at {nw_bin!r}. "
            "Set BENCH_NESTWEAVER or pass --nestweaver-bin."
        )


def extract_referenced_files(nw_response: str, repo_path: str) -> list[str]:
    """
    Parse a NestWeaver JSON response and return the list of unique source file
    paths (absolute) that it references.

    Both ``context`` and ``search`` responses use a ``location`` field with the
    format ``"file_path:line"`` on their result nodes.  ``context`` nests these
    under ``connected``; ``search`` nests them under ``results``.
    """
    if not nw_response.strip():
        return []

    try:
        data = json.loads(nw_response)
    except json.JSONDecodeError:
        return []

    nodes: list[dict] = []
    # context response shape
    if "connected" in data:
        nodes.extend(data["connected"])
        nodes.extend(data.get("seeds", []))
    # search response shape
    if "results" in data:
        nodes.extend(data["results"])

    seen: set[str] = set()
    files: list[str] = []
    repo = Path(repo_path)

    for node in nodes:
        loc = node.get("location") or node.get("file_path") or ""
        if not loc:
            continue
        # location is typically "path/to/file.rs:42" — strip the trailing :line
        file_rel = loc.rsplit(":", 1)[0] if ":" in loc else loc
        if not file_rel or file_rel in seen:
            continue
        seen.add(file_rel)
        full = repo / file_rel
        if full.is_file():
            files.append(str(full))

    return files


def _git_grep_files(query: str, repo_path: str) -> list[str]:
    """Fallback: find files matching the query term via git grep."""
    try:
        out = subprocess.check_output(
            ["git", "-C", repo_path, "grep", "-l", "-i", query],
            text=True, timeout=30, stderr=subprocess.DEVNULL,
        ).strip()
        if not out:
            return []
        return [str(Path(repo_path) / f) for f in out.splitlines()]
    except (subprocess.CalledProcessError, subprocess.TimeoutExpired):
        return []


def _read_file_tokens(paths: list[str], encoder) -> int:
    """Sum tokens across a list of file paths."""
    total = 0
    for p in paths:
        try:
            content = Path(p).read_text(errors="replace")
            total += count_tokens(content, encoder)
        except Exception:
            continue
    return total


def measure_savings(
    queries_path: Path,
    index_dir: Path,
    repos_dir: Path,
    output_path: Path,
    encoder,
    nw_bin: str,
) -> list[dict]:
    with queries_path.open() as f:
        queries_data = json.load(f)

    savings: list[dict] = []

    for repo_entry in queries_data["repos"]:
        repo_name = repo_entry["name"]
        repo_path = str(repos_dir / repo_name)
        db = str(index_dir / f"nestweaver-{repo_name}" / "bench.lbug")

        if not Path(db).exists():
            print(f"  [{repo_name}] skipping — no index at {db}")
            continue

        for nw_kind, queries in [("search", repo_entry.get("search_queries", [])),
                                  ("context", repo_entry.get("context_queries", []))]:

            for query in queries:
                t0 = time.monotonic()
                nw_response = nestweaver_query(query, db, kind=nw_kind, nw_bin=nw_bin)
                latency_ms = (time.monotonic() - t0) * 1000

                response_tokens = count_tokens(nw_response, encoder)

                # Extract the files NestWeaver actually referenced
                ref_files = extract_referenced_files(nw_response, repo_path)

                # Fallback: if no files extracted, use git grep
                fallback = False
                if not ref_files:
                    ref_files = _git_grep_files(query, repo_path)
                    fallback = True

                raw_tokens = _read_file_tokens(ref_files, encoder)

                if raw_tokens > 0:
                    savings_pct = (1 - response_tokens / raw_tokens) * 100
                else:
                    savings_pct = 0.0

                record = {
                    "repo": repo_name,
                    "query": query,
                    "kind": nw_kind,
                    "referenced_files": len(ref_files),
                    "fallback": fallback,
                    "raw_tokens": raw_tokens,
                    "response_tokens": response_tokens,
                    "token_savings_pct": round(savings_pct, 2),
                    "latency_ms": round(latency_ms, 1),
                }
                savings.append(record)

                print(
                    f"  [{repo_name}] {query!r:40s} "
                    f"files={len(ref_files):>3}  "
                    f"raw={raw_tokens:>7,}  nw={response_tokens:>6,}  "
                    f"savings={savings_pct:5.1f}%  "
                    f"latency={latency_ms:.0f}ms"
                    f"{'  (fallback)' if fallback else ''}"
                )

    return savings


def summarise(savings: list[dict]) -> dict:
    if not savings:
        return {}

    total_raw = sum(r["raw_tokens"] for r in savings)
    total_nw = sum(r["response_tokens"] for r in savings)
    overall_savings = (1 - total_nw / total_raw) * 100 if total_raw else 0.0
    avg_savings = (
        sum(r["token_savings_pct"] for r in savings) / len(savings)
        if savings else 0.0
    )

    by_repo: dict[str, dict] = {}
    for r in savings:
        repo = r["repo"]
        by_repo.setdefault(repo, {"raw": 0, "nw": 0, "count": 0, "pcts": []})
        by_repo[repo]["raw"] += r["raw_tokens"]
        by_repo[repo]["nw"] += r["response_tokens"]
        by_repo[repo]["count"] += 1
        by_repo[repo]["pcts"].append(r["token_savings_pct"])

    repo_summary = {}
    for repo, v in by_repo.items():
        repo_avg = sum(v["pcts"]) / len(v["pcts"]) if v["pcts"] else 0.0
        repo_summary[repo] = {
            "raw_tokens": v["raw"],
            "response_tokens": v["nw"],
            "token_savings_pct": round(
                (1 - v["nw"] / v["raw"]) * 100 if v["raw"] else 0.0, 2
            ),
            "avg_savings_pct": round(repo_avg, 2),
            "query_count": v["count"],
        }

    return {
        "overall": {
            "total_raw_tokens": total_raw,
            "total_response_tokens": total_nw,
            "token_savings_pct": round(overall_savings, 2),
            "avg_savings_pct": round(avg_savings, 2),
            "query_count": len(savings),
        },
        "by_repo": repo_summary,
    }


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--queries",
        required=True,
        type=Path,
        help="Path to queries.json.",
    )
    parser.add_argument(
        "--index-dir",
        required=True,
        type=Path,
        help="Directory containing NestWeaver index databases.",
    )
    parser.add_argument(
        "--repos-dir",
        required=True,
        type=Path,
        help="Directory containing cloned repos.",
    )
    parser.add_argument(
        "--output",
        default=Path("benchmarks/token-savings.json"),
        type=Path,
        help="Where to write the token-savings output JSON.",
    )
    parser.add_argument(
        "--nestweaver-bin",
        type=str,
        default=None,
        help="Path to nestweaver binary (default: $BENCH_NESTWEAVER or 'nestweaver').",
    )
    args = parser.parse_args()

    nw_bin = (
        args.nestweaver_bin
        or os.environ.get("BENCH_NESTWEAVER")
        or "nestweaver"
    )

    ensure_tiktoken()

    import tiktoken  # noqa: PLC0415 — imported after ensure_tiktoken()

    encoder = tiktoken.get_encoding("cl100k_base")

    print(f"Measuring token savings (nestweaver: {nw_bin}) …")
    savings = measure_savings(
        args.queries, args.index_dir, args.repos_dir,
        args.output, encoder, nw_bin,
    )
    summary = summarise(savings)

    output = {"summary": summary, "per_query": savings}

    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(output, indent=2))
    print(f"\nWrote {args.output}")
    print(
        f"Overall savings: {summary.get('overall', {}).get('token_savings_pct', 'n/a')}% "
        f"(avg per-query: {summary.get('overall', {}).get('avg_savings_pct', 'n/a')}%) "
        f"across {summary.get('overall', {}).get('query_count', 0)} queries"
    )


if __name__ == "__main__":
    main()
