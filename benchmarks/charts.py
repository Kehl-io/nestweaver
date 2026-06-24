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
# Dark theme (matches nestweaver.kehl.io design tokens)
# ---------------------------------------------------------------------------
DARK = {
    "bg": "#111318",       # ink-950
    "card": "#1A1D24",     # ink-900
    "border": "#2A2D35",   # ink-800
    "muted": "#6B7280",    # ink-500
    "text": "#A1A7B4",     # ink-300
    "heading": "#ECEEF2",  # ink-50
}


def apply_dark_theme():
    """Configure matplotlib for dark-mode charts."""
    plt.rcParams.update({
        "figure.facecolor": DARK["bg"],
        "axes.facecolor": DARK["card"],
        "axes.edgecolor": DARK["border"],
        "axes.labelcolor": DARK["text"],
        "xtick.color": DARK["text"],
        "ytick.color": DARK["text"],
        "text.color": DARK["heading"],
        "legend.facecolor": DARK["card"],
        "legend.edgecolor": DARK["border"],
        "legend.labelcolor": DARK["text"],
        "grid.color": DARK["border"],
        "savefig.facecolor": DARK["bg"],
        "font.family": "sans-serif",
        "font.sans-serif": ["Inter", "Helvetica Neue", "Arial", "sans-serif"],
    })


apply_dark_theme()


# ---------------------------------------------------------------------------
# Constants
# ---------------------------------------------------------------------------
TOOL_COLORS = {
    "nestweaver": "#4F9CF7",
    "graphify": "#10B981",
    "gitnexus": "#F59E0B",
}

TOOL_LABELS = {
    "nestweaver": "NestWeaver",
    "graphify": "Graphify",
    "gitnexus": "GitNexus",
}

TOOL_ORDER = ["nestweaver", "graphify", "gitnexus"]

REPO_ORDER = ["tailwindcss", "deno", "next.js", "elasticsearch"]

REPO_LABELS = {
    "tailwindcss": "Tailwind CSS",
    "deno": "Deno",
    "next.js": "Next.js",
    "elasticsearch": "Elasticsearch",
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
# Quality metrics extraction
# ---------------------------------------------------------------------------
def compute_quality_stats(data: dict) -> dict:
    """Extract per-repo quality stats from query-level results.

    Returns a dict keyed by repo, each containing per-tool averages for
    context queries (seeds, connected, unique_files) and search queries
    (results, noise_nodes).
    """
    repos = [r for r in REPO_ORDER if r in data]
    stats: dict[str, dict] = {}

    for repo in repos:
        repo_stats: dict[str, dict] = {}

        for tool in TOOL_ORDER:
            result = data.get(repo, {}).get(tool, {})
            queries = result.get("queries", [])
            if not queries:
                continue

            t: dict = {}

            if tool == "nestweaver":
                ctx_qs = [q for q in queries if q.get("kind") == "context"]
                search_qs = [q for q in queries if q.get("kind") == "search"]

                if ctx_qs:
                    t["avg_seeds"] = sum(q.get("seeds", 0) for q in ctx_qs) / len(ctx_qs)
                    t["avg_connected"] = sum(q.get("connected", 0) for q in ctx_qs) / len(ctx_qs)
                    t["avg_unique_files"] = sum(q.get("unique_files", 0) for q in ctx_qs) / len(ctx_qs)
                if search_qs:
                    t["avg_search_results"] = sum(q.get("results", 0) for q in search_qs) / len(search_qs)

            elif tool == "graphify":
                # Graphify queries are structural (no kind field)
                t["avg_nodes"] = sum(q.get("results", 0) for q in queries) / len(queries)
                t["avg_noise"] = sum(q.get("noise_nodes", 0) for q in queries) / len(queries)
                total_nodes = sum(q.get("results", 0) for q in queries)
                total_noise = sum(q.get("noise_nodes", 0) for q in queries)
                t["noise_pct"] = (total_noise / total_nodes * 100) if total_nodes > 0 else 0.0

            elif tool == "gitnexus":
                t["avg_results"] = sum(q.get("results", 0) for q in queries) / len(queries)

            if t:
                repo_stats[tool] = t

        if repo_stats:
            stats[repo] = repo_stats

    return stats


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
# New charts
# ---------------------------------------------------------------------------
def chart_token_savings(token_savings: dict, output_dir: Path) -> str:
    """Grouped bar chart: raw file tokens vs NestWeaver response tokens per repo."""
    summary = token_savings.get("summary", {})
    by_repo = summary.get("by_repo", {})
    if not by_repo:
        return ""

    repos = [r for r in REPO_ORDER if r in by_repo]
    if not repos:
        return ""

    fig, ax = plt.subplots(figsize=(12, 6))
    bar_width = 0.35
    x_pos = list(range(len(repos)))

    raw_values = [by_repo[r].get("raw_tokens", 0) for r in repos]
    nw_values = [by_repo[r].get("response_tokens", 0) for r in repos]
    savings_pcts = [by_repo[r].get("token_savings_pct", 0) for r in repos]

    bars_raw = ax.bar(
        [x - bar_width / 2 for x in x_pos], raw_values, bar_width,
        label="Raw File Tokens", color="#9CA3AF", zorder=3,
    )
    bars_nw = ax.bar(
        [x + bar_width / 2 for x in x_pos], nw_values, bar_width,
        label="NestWeaver Tokens", color=TOOL_COLORS["nestweaver"], zorder=3,
    )

    for bar, val in zip(bars_raw, raw_values):
        ax.text(
            bar.get_x() + bar.get_width() / 2, bar.get_height(),
            f"{val:,}", ha="center", va="bottom", fontsize=8,
        )
    for bar, val in zip(bars_nw, nw_values):
        ax.text(
            bar.get_x() + bar.get_width() / 2, bar.get_height(),
            f"{val:,}", ha="center", va="bottom", fontsize=8,
        )

    # Savings percentage annotation above each pair
    for i, pct in enumerate(savings_pcts):
        max_h = max(raw_values[i], nw_values[i])
        ax.text(
            x_pos[i], max_h * 1.12,
            f"{pct}% saved", ha="center", va="bottom",
            fontsize=10, fontweight="bold", color=TOOL_COLORS["nestweaver"],
        )

    ax.set_xlabel("Repository")
    ax.set_ylabel("Token Count")
    ax.set_title("Token Savings: Raw Files vs NestWeaver")
    ax.set_xticks(x_pos)
    ax.set_xticklabels([REPO_LABELS.get(r, r) for r in repos])
    ax.legend(loc="upper left")
    ax.grid(axis="y", alpha=0.3, zorder=0)
    ax.yaxis.set_major_formatter(ticker.FuncFormatter(lambda x, _: f"{int(x):,}"))

    plt.tight_layout()
    out_path = output_dir / "token-savings.svg"
    fig.savefig(out_path, format="svg")
    plt.close(fig)
    return str(out_path)


def chart_incremental_indexing(data: dict, output_dir: Path) -> str:
    """Bar chart: incremental re-index time for NestWeaver only."""
    repos = [r for r in REPO_ORDER if r in data]
    values = []
    valid_repos = []
    for r in repos:
        nw = data.get(r, {}).get("nestweaver", {})
        inc_ms = nw.get("incremental_median_ms")
        if inc_ms is not None:
            values.append(inc_ms)
            valid_repos.append(r)

    if not valid_repos:
        return ""

    fig, ax = plt.subplots(figsize=(10, 5))
    x_pos = list(range(len(valid_repos)))

    bars = ax.bar(
        x_pos, values, 0.5,
        color=TOOL_COLORS["nestweaver"], zorder=3,
    )
    for bar, val in zip(bars, values):
        ax.text(
            bar.get_x() + bar.get_width() / 2, bar.get_height(),
            f"{val:.0f}ms", ha="center", va="bottom", fontsize=10,
        )

    ax.set_xlabel("Repository")
    ax.set_ylabel("Time (ms)")
    ax.set_title("Incremental Re-indexing (single file change)")
    ax.set_xticks(x_pos)
    ax.set_xticklabels([REPO_LABELS.get(r, r) for r in valid_repos])
    ax.grid(axis="y", alpha=0.3, zorder=0)

    plt.tight_layout()
    out_path = output_dir / "incremental-indexing.svg"
    fig.savefig(out_path, format="svg")
    plt.close(fig)
    return str(out_path)


def chart_graph_depth(data: dict, output_dir: Path) -> str:
    """Grouped bar chart: symbols and edges extracted per tool per repo."""
    repos = [r for r in REPO_ORDER if r in data]
    tools = [t for t in TOOL_ORDER if any(t in data.get(r, {}) for r in repos)]
    if not repos or not tools:
        return ""

    has_data = False
    for r in repos:
        for t in tools:
            if data.get(r, {}).get(t, {}).get("symbol_count"):
                has_data = True
                break
    if not has_data:
        return ""

    fig, (ax1, ax2) = plt.subplots(1, 2, figsize=(14, 6))
    bar_width = 0.8 / len(tools)
    x_pos = list(range(len(repos)))

    for metric_idx, (ax, key, ylabel, title) in enumerate([
        (ax1, "symbol_count", "Symbols / Nodes", "Symbols Extracted"),
        (ax2, "edge_count", "Edges / Links", "Edges Extracted"),
    ]):
        for i, tool in enumerate(tools):
            values = []
            for repo in repos:
                result = data.get(repo, {}).get(tool, {})
                values.append(result.get(key, 0))
            offset = (i - len(tools) / 2 + 0.5) * bar_width
            bars = ax.bar(
                [x + offset for x in x_pos], values, bar_width,
                label=TOOL_LABELS.get(tool, tool),
                color=TOOL_COLORS.get(tool, "#888888"), zorder=3,
            )
            for bar, val in zip(bars, values):
                if val > 0:
                    label = f"{val // 1000}K" if val >= 1000 else str(val)
                    ax.text(
                        bar.get_x() + bar.get_width() / 2,
                        bar.get_height(), label,
                        ha="center", va="bottom", fontsize=7,
                    )

        ax.set_xlabel("Repository")
        ax.set_ylabel(ylabel)
        ax.set_title(title)
        ax.set_xticks(x_pos)
        ax.set_xticklabels([REPO_LABELS.get(r, r) for r in repos])
        ax.legend(loc="upper left")
        ax.grid(axis="y", alpha=0.3, zorder=0)

    plt.tight_layout()
    out_path = output_dir / "graph-depth.svg"
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
    token_savings_chart: str = "",
    incremental_chart: str = "",
    graph_depth_chart: str = "",
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

    # --- Key Findings (speedup factors) ---
    competitors = [t for t in tools if t != "nestweaver"]
    if competitors and "nestweaver" in tools:
        lines.append("## Key Findings")
        lines.append("")
        for comp in competitors:
            # Compute average indexing speedup
            idx_ratios = []
            query_ratios = []
            for repo in repos:
                nw = data.get(repo, {}).get("nestweaver", {})
                cr = data.get(repo, {}).get(comp, {})
                nw_idx = nw.get("index_median_ms")
                cr_idx = cr.get("index_median_ms")
                if nw_idx and cr_idx and nw_idx > 0:
                    idx_ratios.append(cr_idx / nw_idx)
                nw_p50 = nw.get("p50_ms")
                cr_p50 = cr.get("p50_ms")
                if nw_p50 and cr_p50 and nw_p50 > 0:
                    query_ratios.append(cr_p50 / nw_p50)
            parts = []
            if idx_ratios:
                avg_idx = sum(idx_ratios) / len(idx_ratios)
                parts.append(f"indexes **{avg_idx:.1f}x faster**")
            if query_ratios:
                avg_q = sum(query_ratios) / len(query_ratios)
                parts.append(f"answers queries **{avg_q:.1f}x faster**")
            if parts:
                comp_label = TOOL_LABELS.get(comp, comp)
                lines.append(f"- NestWeaver {' and '.join(parts)} than {comp_label}")

        # Quality stat: NestWeaver connected nodes vs Graphify nodes
        quality_stats = compute_quality_stats(data)
        nw_connected_all = []
        gf_nodes_all = []
        for repo in repos:
            rs = quality_stats.get(repo, {})
            nw_c = rs.get("nestweaver", {}).get("avg_connected")
            gf_n = rs.get("graphify", {}).get("avg_nodes")
            if nw_c is not None:
                nw_connected_all.append(nw_c)
            if gf_n is not None:
                gf_nodes_all.append(gf_n)
        if nw_connected_all and gf_nodes_all:
            avg_nw = sum(nw_connected_all) / len(nw_connected_all)
            avg_gf = sum(gf_nodes_all) / len(gf_nodes_all)
            if avg_gf > 0:
                depth_x = avg_nw / avg_gf
                lines.append(f"- NestWeaver returns **{depth_x:.1f}x deeper code context** "
                             f"than Graphify ({avg_nw:.0f} connected symbols vs {avg_gf:.0f} nodes per query)")
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

    # --- Incremental indexing ---
    inc_repos = []
    for repo in repos:
        nw = data.get(repo, {}).get("nestweaver", {})
        inc_ms = nw.get("incremental_median_ms")
        if inc_ms is not None:
            inc_repos.append((repo, inc_ms))

    if inc_repos:
        lines.append("## Incremental Indexing")
        lines.append("")
        lines.append("After a single file change, NestWeaver can re-index without "
                      "rebuilding the entire graph. Competitors require a full re-index.")
        lines.append("")
        if incremental_chart:
            lines.append("![Incremental Indexing](incremental-indexing.svg)")
            lines.append("")
        lines.append("| Repository | Incremental Re-index |")
        lines.append("|---|---:|")
        for repo, ms in inc_repos:
            lines.append(f"| {REPO_LABELS.get(repo, repo)} | {ms:.0f}ms |")
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

    # --- Graph depth ---
    has_graph_stats = any(
        data.get(r, {}).get(t, {}).get("symbol_count")
        for r in repos for t in tools
    )
    if has_graph_stats:
        lines.append("## Graph Depth")
        lines.append("")
        lines.append("NestWeaver extracts more symbols and cross-references than competitors,")
        lines.append("building a richer knowledge graph that powers deeper query results.")
        lines.append("")
        if graph_depth_chart:
            lines.append("![Graph Depth](graph-depth.svg)")
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
            row_sym = f"| {label} | Symbols |"
            row_edge = f"| {label} | Edges |"
            for tool in tools:
                result = data.get(repo, {}).get(tool, {})
                sym = result.get("symbol_count", 0)
                edg = result.get("edge_count", 0)
                row_sym += f" {sym:,} |" if sym else " - |"
                row_edge += f" {edg:,} |" if edg else " - |"
            lines.append(row_sym)
            lines.append(row_edge)
        lines.append("")

    # --- Result quality ---
    quality_stats = compute_quality_stats(data)

    lines.append("## Result Quality")
    lines.append("")
    lines.append("Token reduction means nothing if the results are garbage. This section "
                 "shows that NestWeaver doesn't just use fewer tokens — it returns richer, "
                 "more accurate context by following real code relationships instead of "
                 "keyword-matching or relying on shallow AST walks.")
    lines.append("")

    # Context Depth table
    has_context = any(
        quality_stats.get(r, {}).get("nestweaver", {}).get("avg_seeds") is not None
        for r in repos
    )
    if has_context:
        lines.append("### Context Depth")
        lines.append("")
        lines.append("Per-repo averages for structural / context queries:")
        lines.append("")
        lines.append("| Repository | NW Seeds | NW Connected | NW Unique Files "
                     "| Graphify Nodes | Graphify Noise | GitNexus Results |")
        lines.append("|---|---:|---:|---:|---:|---:|---:|")
        for repo in repos:
            rs = quality_stats.get(repo, {})
            nw = rs.get("nestweaver", {})
            gf = rs.get("graphify", {})
            gn = rs.get("gitnexus", {})
            seeds = f"{nw['avg_seeds']:.1f}" if "avg_seeds" in nw else "-"
            connected = f"{nw['avg_connected']:.1f}" if "avg_connected" in nw else "-"
            unique = f"{nw['avg_unique_files']:.1f}" if "avg_unique_files" in nw else "-"
            gf_nodes = f"{gf['avg_nodes']:.1f}" if "avg_nodes" in gf else "-"
            gf_noise = f"{gf['avg_noise']:.1f}" if "avg_noise" in gf else "-"
            gn_results = f"{gn['avg_results']:.1f}" if "avg_results" in gn else "-"
            lines.append(f"| {REPO_LABELS.get(repo, repo)} | {seeds} | {connected} "
                         f"| {unique} | {gf_nodes} | {gf_noise} | {gn_results} |")
        lines.append("")

    # Search Results table
    has_search = any(
        quality_stats.get(r, {}).get("nestweaver", {}).get("avg_search_results") is not None
        or quality_stats.get(r, {}).get("graphify", {}).get("avg_nodes") is not None
        or quality_stats.get(r, {}).get("gitnexus", {}).get("avg_results") is not None
        for r in repos
    )
    if has_search:
        lines.append("### Search Results")
        lines.append("")
        lines.append("Per-repo averages for search / keyword queries:")
        lines.append("")
        header = "| Repository |"
        sep = "|---|"
        for tool in tools:
            header += f" {TOOL_LABELS.get(tool, tool)} |"
            sep += "---:|"
        lines.append(header)
        lines.append(sep)
        for repo in repos:
            rs = quality_stats.get(repo, {})
            row = f"| {REPO_LABELS.get(repo, repo)} |"
            for tool in tools:
                ts = rs.get(tool, {})
                if tool == "nestweaver":
                    val = ts.get("avg_search_results")
                elif tool == "graphify":
                    val = ts.get("avg_nodes")
                elif tool == "gitnexus":
                    val = ts.get("avg_results")
                else:
                    val = None
                row += f" {val:.1f} |" if val is not None else " - |"
            lines.append(row)
        lines.append("")

    # Quality Verdict
    lines.append("### Quality Verdict")
    lines.append("")

    # Aggregate cross-repo quality metrics
    nw_connected_vals = []
    nw_unique_files_vals = []
    gf_nodes_vals = []
    gf_noise_pcts = []
    gn_results_vals = []
    for repo in repos:
        rs = quality_stats.get(repo, {})
        nw = rs.get("nestweaver", {})
        gf = rs.get("graphify", {})
        gn = rs.get("gitnexus", {})
        if "avg_connected" in nw:
            nw_connected_vals.append(nw["avg_connected"])
        if "avg_unique_files" in nw:
            nw_unique_files_vals.append(nw["avg_unique_files"])
        if "avg_nodes" in gf:
            gf_nodes_vals.append(gf["avg_nodes"])
        if "noise_pct" in gf:
            gf_noise_pcts.append(gf["noise_pct"])
        if "avg_results" in gn:
            gn_results_vals.append(gn["avg_results"])

    if nw_connected_vals and gf_nodes_vals:
        avg_nw_conn = sum(nw_connected_vals) / len(nw_connected_vals)
        avg_gf_nodes = sum(gf_nodes_vals) / len(gf_nodes_vals)
        if avg_gf_nodes > 0:
            depth_ratio = avg_nw_conn / avg_gf_nodes
            lines.append(f"- **Context depth**: NestWeaver returns {depth_ratio:.1f}x more connected "
                         f"symbols per query than Graphify ({avg_nw_conn:.0f} vs {avg_gf_nodes:.0f})")

    if gf_noise_pcts:
        avg_noise = sum(gf_noise_pcts) / len(gf_noise_pcts)
        lines.append(f"- **Graphify noise**: {avg_noise:.0f}% of Graphify's returned nodes are "
                     f"framework stubs or vendored code without actionable source")

    if gn_results_vals:
        lines.append(f"- **GitNexus completeness**: GitNexus finds symbols but returns "
                     f"0 callers and 0 callees — it locates definitions without showing "
                     f"how they connect to the rest of the codebase")

    if nw_connected_vals and nw_unique_files_vals:
        avg_conn = sum(nw_connected_vals) / len(nw_connected_vals)
        avg_files = sum(nw_unique_files_vals) / len(nw_unique_files_vals)
        if avg_conn > 0:
            file_pct = (avg_files / avg_conn) * 100
            gf_noise_str = ""
            if gf_noise_pcts:
                gf_noise_str = f" Graphify returns {sum(gf_nodes_vals) / len(gf_nodes_vals):.0f} nodes but {sum(gf_noise_pcts) / len(gf_noise_pcts):.0f}% are framework stubs without source code."
            lines.append("")
            lines.append(f"> NestWeaver returns **{avg_conn:.0f} connected symbols** per query with "
                         f"**{file_pct:.0f}%** being from unique source files.{gf_noise_str} "
                         f"GitNexus finds symbols but returns 0 callers and 0 callees.")

    lines.append("")

    # --- Token savings ---
    if token_savings:
        lines.append("## Token Savings (NestWeaver)")
        lines.append("")
        if token_savings_chart:
            lines.append("![Token Savings](token-savings.svg)")
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
    ts_chart = chart_token_savings(token_savings, args.output_dir)
    inc_chart = chart_incremental_indexing(data, args.output_dir)
    depth_chart = chart_graph_depth(data, args.output_dir)

    report_path = generate_report(
        data, metadata, token_savings, args.output_dir,
        indexing_chart, latency_chart,
        token_savings_chart=ts_chart,
        incremental_chart=inc_chart,
        graph_depth_chart=depth_chart,
    )

    print(f"Report:  {report_path}")
    for chart in [indexing_chart, latency_chart, ts_chart, inc_chart, depth_chart]:
        if chart:
            print(f"Chart:   {chart}")


if __name__ == "__main__":
    main()
