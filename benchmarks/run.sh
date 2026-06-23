#!/usr/bin/env bash
# run.sh — NestWeaver benchmark orchestrator
# Fully isolated: builds NestWeaver to a local prefix, installs competitors
# locally, uses a dedicated daemon instance. Nothing touches your global
# NestWeaver installation.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
QUERIES="$SCRIPT_DIR/queries.json"

# ---------------------------------------------------------------------------
# Directories — everything under BENCH_ROOT
# ---------------------------------------------------------------------------
BENCH_ROOT="${BENCH_ROOT:-/private/tmp/nestweaver-bench}"
REPOS_DIR="$BENCH_ROOT/repos"
INDEX_DIR="$BENCH_ROOT/indexes"
RESULTS_DIR="$BENCH_ROOT/results"
REPORT_DIR="$BENCH_ROOT/report"
VENVS_DIR="$BENCH_ROOT/venvs"
NODE_DIR="$BENCH_ROOT/node"
BIN_DIR="$BENCH_ROOT/bin"
LOCAL_PREFIX="$BENCH_ROOT/local"

mkdir -p "$REPOS_DIR" "$INDEX_DIR" "$RESULTS_DIR" "$REPORT_DIR" \
         "$VENVS_DIR" "$NODE_DIR" "$BIN_DIR" "$LOCAL_PREFIX"

NUM_RUNS="${NUM_RUNS:-3}"
export NUM_RUNS REPOS_DIR INDEX_DIR RESULTS_DIR REPORT_DIR BIN_DIR BENCH_ROOT QUERIES REPO_ROOT

# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------
info()  { printf '\033[1;34m[bench]\033[0m %s\n' "$*"; }
warn()  { printf '\033[1;33m[bench]\033[0m %s\n' "$*"; }
die()   { printf '\033[1;31m[bench]\033[0m %s\n' "$*" >&2; exit 1; }

check_dep() {
    command -v "$1" &>/dev/null || die "Missing dependency: $1"
}

cleanup() {
    info "Cleaning up benchmark daemon..."
    if [[ -n "${BENCH_NESTWEAVER:-}" ]] && [[ -x "$BENCH_NESTWEAVER" ]]; then
        "$BENCH_NESTWEAVER" daemon stop --quiet 2>/dev/null || true
    fi
}
trap cleanup EXIT

# ---------------------------------------------------------------------------
# 1. Check dependencies
# ---------------------------------------------------------------------------
info "Checking dependencies..."
for dep in python3 git cargo jq curl; do
    check_dep "$dep"
done
python3 -m pip --version &>/dev/null || die "Missing dependency: pip (python3 -m pip)"

# ---------------------------------------------------------------------------
# 2. Clone repos (shallow, skip if present)
# ---------------------------------------------------------------------------
REPO_DATA=$(python3 -c "
import json
data = json.load(open('$QUERIES'))
for r in data['repos']:
    print(r['name'], r['url'])
")
REPO_NAMES=$(echo "$REPO_DATA" | awk '{print $1}')

info "Cloning repos (shallow, --depth 1)..."
echo "$REPO_DATA" | while read -r name url; do
    dest="$REPOS_DIR/$name"
    if [[ -d "$dest/.git" ]]; then
        info "  $name — already cloned, skipping"
    else
        info "  $name — cloning from $url"
        git clone --depth 1 "$url" "$dest"
    fi
done

# ---------------------------------------------------------------------------
# 3. Build NestWeaver to local prefix (isolated from global install)
# ---------------------------------------------------------------------------
info "Building NestWeaver from source (isolated to $LOCAL_PREFIX)..."
FEATURES="embed"
if [[ "$(uname -s)" == "Darwin" ]] && sysctl -n machdep.cpu.brand_string 2>/dev/null | grep -q Apple; then
    FEATURES="embed,metal"
    info "  Detected Apple Silicon — enabling Metal GPU acceleration"
fi
cargo install --path "$REPO_ROOT" --root "$LOCAL_PREFIX" --features "$FEATURES" 2>&1 | tail -3
BENCH_NESTWEAVER="$LOCAL_PREFIX/bin/nestweaver"
[[ -x "$BENCH_NESTWEAVER" ]] || die "Build failed — $BENCH_NESTWEAVER not found"
NW_VERSION=$("$BENCH_NESTWEAVER" --version 2>/dev/null || echo "dev")
info "  nestweaver $NW_VERSION at $BENCH_NESTWEAVER"
export BENCH_NESTWEAVER

# ---------------------------------------------------------------------------
# 4. Install competitors (all isolated under BENCH_ROOT)
# ---------------------------------------------------------------------------
info "Installing competitors..."

# Python venv for benchmark scripts (charts, token_savings)
BENCH_VENV="$VENVS_DIR/bench"
if [[ ! -f "$BENCH_VENV/bin/activate" ]]; then
    python3 -m venv "$BENCH_VENV"
fi
"$BENCH_VENV/bin/pip" install --quiet matplotlib tiktoken 2>/dev/null \
    || warn "  matplotlib/tiktoken install failed (charts/token-savings may not work)"
BENCH_PYTHON="$BENCH_VENV/bin/python3"
export BENCH_PYTHON

# Graphify — Python venv (PyPI package is "graphifyy" with double-y, binary is "graphify")
GRAPHIFY_VENV="$VENVS_DIR/graphify"
if [[ ! -f "$GRAPHIFY_VENV/bin/graphify" ]]; then
    if [[ ! -f "$GRAPHIFY_VENV/bin/activate" ]]; then
        python3 -m venv "$GRAPHIFY_VENV"
    fi
    "$GRAPHIFY_VENV/bin/pip" install --quiet graphifyy 2>/dev/null \
        || warn "  graphifyy pip install failed"
fi
GRAPHIFY_BIN="$GRAPHIFY_VENV/bin/graphify"

# GitNexus — local npm install
GITNEXUS_DIR="$NODE_DIR/gitnexus"
if [[ ! -f "$GITNEXUS_DIR/node_modules/.bin/gitnexus" ]]; then
    mkdir -p "$GITNEXUS_DIR"
    npm install --prefix "$GITNEXUS_DIR" gitnexus 2>/dev/null \
        || warn "  gitnexus npm install failed"
fi
GITNEXUS_BIN="$GITNEXUS_DIR/node_modules/.bin/gitnexus"

export GRAPHIFY_BIN GITNEXUS_BIN

# ---------------------------------------------------------------------------
# 5. Record metadata
# ---------------------------------------------------------------------------
info "Recording metadata..."
METADATA="$RESULTS_DIR/metadata.json"

REPO_META=$(python3 -c "
import json, subprocess, os
repos_dir = '$REPOS_DIR'
data = json.load(open('$QUERIES'))
meta = []
for r in data['repos']:
    name = r['name']
    repo_path = os.path.join(repos_dir, name)
    sha = subprocess.check_output(
        ['git', '-C', repo_path, 'rev-parse', 'HEAD'], text=True
    ).strip()
    file_count = subprocess.check_output(
        ['git', '-C', repo_path, 'ls-files'], text=True
    ).count('\n')
    meta.append({
        'name': name,
        'description': r['description'],
        'sha': sha,
        'file_count': file_count,
    })
print(json.dumps(meta))
")

HW_CORES=$(sysctl -n hw.ncpu 2>/dev/null || nproc 2>/dev/null || echo "unknown")
HW_MEM=$(python3 -c "
try:
    import subprocess
    mem = subprocess.check_output(['sysctl', '-n', 'hw.memsize'], text=True).strip()
    print(f'{int(mem) // (1024**3)} GB')
except Exception:
    print('unknown')
")
HW_ARCH=$(uname -m)
OS_INFO=$(uname -rs)

python3 -c "
import json
metadata = {
    'date': '$(date -u +%Y-%m-%dT%H:%M:%SZ)',
    'hardware': {
        'cores': '$HW_CORES',
        'memory': '$HW_MEM',
        'arch': '$HW_ARCH'
    },
    'os': '$OS_INFO',
    'nestweaver_version': '$NW_VERSION',
    'num_runs': $NUM_RUNS,
    'repos': $REPO_META
}
with open('$METADATA', 'w') as f:
    json.dump(metadata, f, indent=2)
print('  Wrote', '$METADATA')
"

# ---------------------------------------------------------------------------
# 6. Source measure.sh and run benchmarks
# ---------------------------------------------------------------------------
info "Loading measurement functions..."
source "$SCRIPT_DIR/measure.sh"

info "Running benchmarks ($NUM_RUNS runs per measurement)..."
for name in $REPO_NAMES; do
    repo_path="$REPOS_DIR/$name"
    info "━━━ $name ━━━"

    benchmark_nestweaver "$name" "$repo_path"

    if [[ -x "$GRAPHIFY_BIN" ]]; then
        benchmark_graphify "$name" "$repo_path"
    else
        warn "  Skipping graphify (not installed)"
    fi

    if [[ -x "$GITNEXUS_BIN" ]]; then
        benchmark_gitnexus "$name" "$repo_path"
    else
        warn "  Skipping gitnexus (not installed)"
    fi

    # codebase-memory-mcp removed — unreliable install, dead upstream URL
done

# ---------------------------------------------------------------------------
# 6b. Run Graphify's own benchmark (for comparison with their claims)
# ---------------------------------------------------------------------------
info "Running Graphify's self-benchmark..."
for name in $REPO_NAMES; do
    graph_file="$RESULTS_DIR/${name}-graphify-graph.json"
    if [[ -f "$graph_file" ]]; then
        info "  [graphify benchmark] $name"
        "$GRAPHIFY_BIN" benchmark "$graph_file" > "$RESULTS_DIR/${name}-graphify-benchmark.txt" 2>&1 || true
        # Show the key stat
        grep -i "reduction\|fewer\|ratio\|savings" "$RESULTS_DIR/${name}-graphify-benchmark.txt" 2>/dev/null || true
    else
        warn "  Skipping graphify benchmark for $name (no graph.json)"
    fi
done

# ---------------------------------------------------------------------------
# 7. Token savings
# ---------------------------------------------------------------------------
info "Measuring token savings..."
"$BENCH_PYTHON" "$SCRIPT_DIR/token_savings.py" \
    --queries "$QUERIES" \
    --index-dir "$INDEX_DIR" \
    --repos-dir "$REPOS_DIR" \
    --output "$RESULTS_DIR/token-savings.json" \
    --nestweaver-bin "$BENCH_NESTWEAVER" \
    || warn "Token savings measurement failed (non-fatal)"

# ---------------------------------------------------------------------------
# 8. Generate report
# ---------------------------------------------------------------------------
info "Generating report and charts..."
"$BENCH_PYTHON" "$SCRIPT_DIR/charts.py" \
    --results-dir "$RESULTS_DIR" \
    --output-dir "$REPORT_DIR"

# ---------------------------------------------------------------------------
# 9. Summary
# ---------------------------------------------------------------------------
echo ""
info "━━━ Benchmark complete ━━━"
info "Results:  $RESULTS_DIR/"
info "Report:   $REPORT_DIR/benchmark-report.md"
info "Charts:   $REPORT_DIR/*.svg"
echo ""
if [[ -f "$REPORT_DIR/benchmark-report.md" ]]; then
    head -30 "$REPORT_DIR/benchmark-report.md"
fi
