#!/usr/bin/env python3
"""
charts.py — generate benchmark report and SVG charts from results.

Usage:
    python3 benchmarks/charts.py --results-dir /tmp/nestweaver-bench/results \
                                  --output-dir /tmp/nestweaver-bench/report
"""

import argparse
import json
import subprocess
import sys
from pathlib import Path

# Auto-install matplotlib if missing
try:
    import matplotlib
    matplotlib.use("Agg")
    import matplotlib.pyplot as plt
    import matplotlib.ticker as ticker
except ImportError:
    print("matplotlib not found — installing…", flush=True)
    subprocess.check_call(
        [sys.executable, "-m", "pip", "install", "--quiet", "matplotlib"]
    )
    import matplotlib
    matplotlib.use("Agg")
    import matplotlib.pyplot as plt
    import matplotlib.ticker as ticker


# ---------------------------------------------------------------------------
# Constants
# ---------------------------------------------------------------------------
TOOL_COLORS = {
    "nestweaver": "#4F46E5",
    "graphify": "#10B981",
    "gitnexus": "#F59E0B",
    "cbmcp": "#EF4444",
}

TOOL_LABELS = {
    "nestweaver": "NestWeaver",
    "graphify": "Graphify",
    "gitnexus": "GitNexus",
    "cbmcp": "Codebase-Memory-MCP",
}

TOOL_ORDER = ["nestweaver", "graphify", "gitnexus", "cbmcp"]

REPO_ORDER = ["linux", "kubernetes", "react", "rust", "nextjs"]

REPO_LABELS = {
    "linux": "Linux kernel",
    "kubernetes": "Kubernetes",
    "react": "React",
    "rust": "Rust compiler",
    "nextjs": "Next.js",
}


# ---------------------------------------------------------------------------
# Data loading
# ---------------------------------------------------------------------------
def load_results(results_dir: Path) -> dict:
    """Load all per-repo per-tool result JSONs into a nested dict."""
    data: dict[str, dict] = {}  # data[repo][tool] = result dict
    for f in results_dir.glob("*-*.json"):
        if f.name in ("metadata.json", "token-savings.json"):
            continue
        try:
            result = json.loads(f.read_text())
        except (json.JSONDecodeError, OSError):
            continue
        repo = result.get("repo", "")
        tool = result.get("tool", "")
        if repo and tool:
            data.setdefault(repo, {})[tool] = result
    return data


def load_metadata(results_dir: Path) -> dict:
    meta_path = results_dir / "metadata.json"
    if meta_path.exists():
        return json.loads(meta_path.read_text())
    return {}


def load_token_savings(results_dir: Path) -> dict:
    ts_path = results_dir / "token-savings.json"
    if ts_path.exists():
        return json.loads(ts_path.read_text())
    return {}


# ---------------------------------------------------------------------------
# Charts
# ---------------------------------------------------------------------------
def chart_indexing_speed(data: dict, output_dir: Path) -> str:
    """Grouped bar chart: indexing time per tool per repo."""
    fig, ax = plt.subplots(figsize=(12, 6))

    repos = [r for r in REPO_ORDER if r in data]
    tools = [t for t in TOOL_ORDER if any(t in data.get(r, {}) for r in repos)]
    n_tools = len(tools)
    if not repos or not n_tools:
        plt.close(fig)
        return ""

    bar_width = 0.8 / n_tools
    x_pos = list(range(len(repos)))

    for i, tool in enumerate(tools):
        values = []
        for repo in repos:
            result = data.get(repo, {}).get(tool, {})
            values.append(result.get("index_median_ms", 0) / 1000)  # seconds
        offset = (i - n_tools / 2 + 0.5) * bar_width
        bars = ax.bar(
            [x + offset for x in x_pos],
            values,
            bar_width,
            label=TOOL_LABELS.get(tool, tool),
            color=TOOL_COLORS.get(tool, "#888888"),
            zorder=3,
        )
        for bar, val in zip(bars, values):
            if val > 0:
                ax.text(
                    bar.get_x() + bar.get_width() / 2,
                    bar.get_height() + 0.3,
                    f"{val:.1f}s",
                    ha="center",
                    va="bottom",
                    fontsize=8,
                )

    ax.set_xlabel("Repository")
    ax.set_ylabel("Indexing Time (seconds)")
    ax.set_title("Indexing Speed Comparison")
    ax.set_xticks(x_pos)
    ax.set_xticklabels([REPO_LABELS.get(r, r) for r in repos])
    ax.legend(loc="upper left")
    ax.grid(axis="y", alpha=0.3, zorder=0)
    ax.yaxis.set_major_formatter(ticker.FormatStrFormatter("%.0f"))

    plt.tight_layout()
    out_path = output_dir / "indexing-speed.svg"
    fig.savefig(out_path, format="svg")
    plt.close(fig)
    return str(out_path)


def chart_query_latency(data: dict, output_dir: Path) -> str:
    """Grouped bar chart: p50 query latency per tool per repo."""
    fig, ax = plt.subplots(figsize=(12, 6))

    repos = [r for r in REPO_ORDER if r in data]
    tools = [t for t in TOOL_ORDER if any(t in data.get(r, {}) for r in repos)]
    n_tools = len(tools)
    if not repos or not n_tools:
        plt.close(fig)
        return ""

    bar_width = 0.8 / n_tools
    x_pos = list(range(len(repos)))

    for i, tool in enumerate(tools):
        values = []
        for repo in repos:
            result = data.get(repo, {}).get(tool, {})
            values.append(result.get("p50_ms", 0))
        offset = (i - n_tools / 2 + 0.5) * bar_width
        bars = ax.bar(
            [x + offset for x in x_pos],
            values,
            bar_width,
            label=TOOL_LABELS.get(tool, tool),
            color=TOOL_COLORS.get(tool, "#888888"),
            zorder=3,
        )
        for bar, val in zip(bars, values):
            if val > 0:
                ax.text(
                    bar.get_x() + bar.get_width() / 2,
                    bar.get_height() + 5,
                    f"{val}ms",
                    ha="center",
                    va="bottom",
                    fontsize=8,
                )

    ax.set_xlabel("Repository")
    ax.set_ylabel("Query Latency p50 (ms)")
    ax.set_title("Query Latency Comparison (p50)")
    ax.set_xticks(x_pos)
    ax.set_xticklabels([REPO_LABELS.get(r, r) for r in repos])
    ax.legend(loc="upper left")
    ax.grid(axis="y", alpha=0.3, zorder=0)

    plt.tight_layout()
    out_path = output_dir / "query-latency.svg"
    fig.savefig(out_path, format="svg")
    plt.close(fig)
    return str(out_path)


# ---------------------------------------------------------------------------
# Report generation
# ---------------------------------------------------------------------------
def generate_report(
    data: dict,
    metadata: dict,
    token_savings: dict,
    output_dir: Path,
    indexing_chart: str,
    latency_chart: str,
) -> str:
    """Generate the markdown benchmark report."""
    lines: list[str] = []

    repos = [r for r in REPO_ORDER if r in data]
    tools = [t for t in TOOL_ORDER if any(t in data.get(r, {}) for r in repos)]

    # --- Hero stat ---
    nw_linux = data.get("linux", {}).get("nestweaver", {})
    hero_index = nw_linux.get("index_median_ms", "N/A")
    hero_query = nw_linux.get("p50_ms", "N/A")
    if isinstance(hero_index, (int, float)):
        hero_index_str = f"{hero_index / 1000:.1f}s"
    else:
        hero_index_str = str(hero_index)
    hero_query_str = f"{hero_query}ms" if isinstance(hero_query, (int, float)) else str(hero_query)

    lines.append("# NestWeaver Benchmark Report")
    lines.append("")
    lines.append(f"> **Linux kernel**: indexed in **{hero_index_str}**, queries answered in **{hero_query_str}** (p50)")
    lines.append("")

    # --- Environment ---
    lines.append("## Environment")
    lines.append("")
    hw = metadata.get("hardware", {})
    lines.append(f"- **Date**: {metadata.get('date', 'N/A')}")
    lines.append(f"- **OS**: {metadata.get('os', 'N/A')}")
    lines.append(f"- **CPU cores**: {hw.get('cores', 'N/A')}")
    lines.append(f"- **Memory**: {hw.get('memory', 'N/A')}")
    lines.append(f"- **Architecture**: {hw.get('arch', 'N/A')}")
    lines.append(f"- **NestWeaver version**: {metadata.get('nestweaver_version', 'N/A')}")
    lines.append(f"- **Runs per measurement**: {metadata.get('num_runs', 'N/A')}")
    lines.append("")

    # --- Repos table ---
    lines.append("## Repositories")
    lines.append("")
    lines.append("| Repository | SHA (short) | Files | Lines (sample) |")
    lines.append("|---|---|---:|---:|")
    for repo_meta in metadata.get("repos", []):
        name = repo_meta.get("name", "")
        sha = repo_meta.get("sha", "")[:8]
        files = f"{repo_meta.get('file_count', 0):,}"
        line_count = repo_meta.get("line_count_sample", 0)
        line_str = f"{line_count:,}" if line_count else "N/A"
        lines.append(f"| {REPO_LABELS.get(name, name)} | `{sha}` | {files} | {line_str} |")
    lines.append("")

    # --- Indexing speed ---
    lines.append("## Indexing Speed")
    lines.append("")
    if indexing_chart:
        lines.append(f"![Indexing Speed](indexing-speed.svg)")
        lines.append("")

    header = "| Repository |"
    sep = "|---|"
    for tool in tools:
        header += f" {TOOL_LABELS.get(tool, tool)} |"
        sep += "---:|"
    lines.append(header)
    lines.append(sep)

    for repo in repos:
        row = f"| {REPO_LABELS.get(repo, repo)} |"
        for tool in tools:
            result = data.get(repo, {}).get(tool, {})
            ms = result.get("index_median_ms")
            if ms is not None:
                if ms >= 1000:
                    row += f" {ms / 1000:.1f}s |"
                else:
                    row += f" {ms}ms |"
            else:
                row += " - |"
        lines.append(row)
    lines.append("")

    # --- Query latency ---
    lines.append("## Query Latency")
    lines.append("")
    if latency_chart:
        lines.append(f"![Query Latency](query-latency.svg)")
        lines.append("")

    header = "| Repository | Metric |"
    sep = "|---|---|"
    for tool in tools:
        header += f" {TOOL_LABELS.get(tool, tool)} |"
        sep += "---:|"
    lines.append(header)
    lines.append(sep)

    for repo in repos:
        label = REPO_LABELS.get(repo, repo)
        row_p50 = f"| {label} | p50 |"
        row_p95 = f"| {label} | p95 |"
        for tool in tools:
            result = data.get(repo, {}).get(tool, {})
            p50 = result.get("p50_ms")
            p95 = result.get("p95_ms")
            row_p50 += f" {p50}ms |" if p50 is not None else " - |"
            row_p95 += f" {p95}ms |" if p95 is not None else " - |"
        lines.append(row_p50)
        lines.append(row_p95)
    lines.append("")

    # --- Retrieval quality ---
    lines.append("## Retrieval Quality")
    lines.append("")
    lines.append("Average seeds (entry points) returned per query:")
    lines.append("")

    header = "| Repository |"
    sep = "|---|"
    for tool in tools:
        header += f" {TOOL_LABELS.get(tool, tool)} |"
        sep += "---:|"
    lines.append(header)
    lines.append(sep)

    for repo in repos:
        row = f"| {REPO_LABELS.get(repo, repo)} |"
        for tool in tools:
            result = data.get(repo, {}).get(tool, {})
            queries = result.get("queries", [])
            if queries:
                avg_seeds = sum(q.get("seeds", 0) for q in queries) / len(queries)
                row += f" {avg_seeds:.1f} |"
            else:
                row += " - |"
        lines.append(row)
    lines.append("")

    # --- Token savings ---
    if token_savings:
        lines.append("## Token Savings (NestWeaver)")
        lines.append("")
        summary = token_savings.get("summary", {})
        overall = summary.get("overall", {})
        if overall:
            lines.append(
                f"Overall: **{overall.get('token_savings_pct', 'N/A')}%** fewer tokens "
                f"across {overall.get('query_count', 0)} queries "
                f"({overall.get('total_raw_tokens', 0):,} raw vs "
                f"{overall.get('total_response_tokens', 0):,} NestWeaver)")
            lines.append("")

        by_repo = summary.get("by_repo", {})
        if by_repo:
            lines.append("| Repository | Raw Tokens | NW Tokens | Savings |")
            lines.append("|---|---:|---:|---:|")
            for repo in repos:
                repo_data = by_repo.get(repo, {})
                if repo_data:
                    lines.append(
                        f"| {REPO_LABELS.get(repo, repo)} "
                        f"| {repo_data.get('raw_tokens', 0):,} "
                        f"| {repo_data.get('response_tokens', 0):,} "
                        f"| {repo_data.get('token_savings_pct', 0)}% |"
                    )
            lines.append("")

    # --- Methodology ---
    lines.append("## Methodology")
    lines.append("")
    lines.append("- All repos cloned at `--depth 1` (latest commit only)")
    lines.append(f"- Each measurement repeated {metadata.get('num_runs', 3)} times; median reported")
    lines.append("- Indexing: fresh index each run (previous index deleted)")
    lines.append("- Queries: 5 natural-language + 5 exact-symbol queries per repo")
    lines.append("- Latency: wall-clock time via monotonic clock")
    lines.append("- Token savings: counted via tiktoken `cl100k_base` encoding")
    lines.append("")

    # --- Reproduce ---
    lines.append("## Reproduce")
    lines.append("")
    lines.append("```bash")
    lines.append("git clone <nestweaver-repo>")
    lines.append("cd nestweaver")
    lines.append("bash benchmarks/run.sh")
    lines.append("```")
    lines.append("")
    lines.append("Results are written to `/tmp/nestweaver-bench/`. "
                 "Override run count with `NUM_RUNS=5 bash benchmarks/run.sh`.")
    lines.append("")

    report_text = "\n".join(lines)
    report_path = output_dir / "benchmark-report.md"
    report_path.write_text(report_text)
    return str(report_path)


# ---------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------
def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--results-dir",
        required=True,
        type=Path,
        help="Directory containing per-repo per-tool JSON result files and metadata.json",
    )
    parser.add_argument(
        "--output-dir",
        required=True,
        type=Path,
        help="Directory to write charts and the report to",
    )
    args = parser.parse_args()

    args.output_dir.mkdir(parents=True, exist_ok=True)

    data = load_results(args.results_dir)
    metadata = load_metadata(args.results_dir)
    token_savings = load_token_savings(args.results_dir)

    if not data:
        print("No benchmark results found — generating empty report.")

    indexing_chart = chart_indexing_speed(data, args.output_dir)
    latency_chart = chart_query_latency(data, args.output_dir)

    report_path = generate_report(
        data, metadata, token_savings, args.output_dir,
        indexing_chart, latency_chart,
    )

    print(f"Report:  {report_path}")
    if indexing_chart:
        print(f"Chart:   {indexing_chart}")
    if latency_chart:
        print(f"Chart:   {latency_chart}")


if __name__ == "__main__":
    main()
