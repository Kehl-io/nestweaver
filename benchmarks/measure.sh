#!/usr/bin/env bash
# measure.sh — benchmark measurement functions, sourced by run.sh
# Requires: BENCH_NESTWEAVER, REPOS_DIR, INDEX_DIR, RESULTS_DIR, QUERIES,
#           NUM_RUNS, GRAPHIFY_BIN, GITNEXUS_BIN, CBMCP_BIN

# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

# time_ms CMD [ARGS...] — run a command and print elapsed wall-clock milliseconds
time_ms() {
    local start end
    start=$(python3 -c 'import time; print(int(time.monotonic_ns()))')
    "$@" >/dev/null 2>&1
    end=$(python3 -c 'import time; print(int(time.monotonic_ns()))')
    echo $(( (end - start) / 1000000 ))
}

# time_ms_capture CMD [ARGS...] — run a command, print elapsed ms, save output
# to $BENCH_ROOT/.captured_output (file-based to survive subshells).
# Read the output with: CAPTURED_OUTPUT=$(cat "$BENCH_ROOT/.captured_output")
CAPTURED_OUTPUT=""
time_ms_capture() {
    local start end
    start=$(python3 -c 'import time; print(int(time.monotonic_ns()))')
    "$@" >"$BENCH_ROOT/.captured_output" 2>&1 || true
    end=$(python3 -c 'import time; print(int(time.monotonic_ns()))')
    echo $(( (end - start) / 1000000 ))
}

# read_captured — read the last captured output into CAPTURED_OUTPUT
read_captured() {
    CAPTURED_OUTPUT=$(cat "$BENCH_ROOT/.captured_output" 2>/dev/null)
}

# median VAL1 VAL2 VAL3 ... — print the median of integer arguments
median() {
    python3 -c "
import sys
vals = sorted(int(x) for x in sys.argv[1:])
n = len(vals)
if n == 0:
    print(0)
elif n % 2 == 1:
    print(vals[n // 2])
else:
    print((vals[n // 2 - 1] + vals[n // 2]) // 2)
" "$@"
}

# percentile P VAL1 VAL2 ... — print the Pth percentile
percentile() {
    local p="$1"; shift
    python3 -c "
import sys, math
p = int(sys.argv[1])
vals = sorted(int(x) for x in sys.argv[2:])
n = len(vals)
if n == 0:
    print(0)
else:
    k = (p / 100) * (n - 1)
    f = math.floor(k)
    c = math.ceil(k)
    if f == c:
        print(vals[int(k)])
    else:
        print(int(vals[int(f)] * (c - k) + vals[int(c)] * (k - f)))
" "$p" "$@"
}

# Load queries for a given repo from queries.json
# Usage: load_queries REPO_NAME KIND
#   KIND = "nl" or "exact"
# Prints one query per line
load_queries() {
    local repo_name="$1" kind="$2"
    local field
    if [[ "$kind" == "search" ]]; then
        field="search_queries"
    else
        field="context_queries"
    fi
    python3 -c "
import json, os
data = json.load(open(os.environ['QUERIES']))
for r in data['repos']:
    if r['name'] == '$repo_name':
        for q in r['$field']:
            print(q)
        break
"
}

# json_quote STRING — safely JSON-encode a string
json_quote() {
    python3 -c "import json,os,sys; print(json.dumps(sys.argv[1]))" "$1"
}

# ---------------------------------------------------------------------------
# benchmark_nestweaver REPO_NAME REPO_PATH
# Uses daemon mode with Metal GPU acceleration via the isolated build.
# Each repo gets its own DB, so the benchmark daemon is per-repo.
# ---------------------------------------------------------------------------
benchmark_nestweaver() {
    local name="$1" repo_path="$2"
    local db_dir="$INDEX_DIR/nestweaver-$name"
    local result_file="$RESULTS_DIR/${name}-nestweaver.json"
    local nw="$BENCH_NESTWEAVER"

    info "  [nestweaver] benchmarking $name..."

    local db="$db_dir/bench.lbug"

    # --- Indexing (NUM_RUNS, fresh each time) ---
    # Each run uses a separate temp directory so we never destroy a DB
    # that a daemon might reference. The final run becomes the keeper.
    # We capture stdout from the last run to extract graph stats.
    local index_times=()
    local last_index_output=""
    for ((i = 1; i <= NUM_RUNS; i++)); do
        local run_dir="$INDEX_DIR/nestweaver-$name-run$i"
        rm -rf "$run_dir"
        mkdir -p "$run_dir"
        local tmpout
        tmpout=$(mktemp "$BENCH_ROOT/idx.XXXXXX")
        local start end
        start=$(python3 -c 'import time; print(int(time.monotonic_ns()))')
        "$nw" index --db "$run_dir/bench.lbug" --repo "$repo_path" >"$tmpout" 2>&1 || true
        end=$(python3 -c 'import time; print(int(time.monotonic_ns()))')
        local ms=$(( (end - start) / 1000000 ))
        last_index_output=$(cat "$tmpout")
        rm -f "$tmpout"
        # Stop the auto-started daemon so next run's fresh DB doesn't conflict
        "$nw" daemon stop --db "$run_dir/bench.lbug" --quiet 2>/dev/null || true
        index_times+=("$ms")
        info "    index run $i: ${ms}ms"
    done
    local index_median
    index_median=$(median "${index_times[@]}")

    # Keep the last run as the canonical DB for queries
    rm -rf "$db_dir"
    mv "$INDEX_DIR/nestweaver-$name-run$NUM_RUNS" "$db_dir"
    # Clean up other run dirs
    for ((i = 1; i < NUM_RUNS; i++)); do
        rm -rf "$INDEX_DIR/nestweaver-$name-run$i"
    done

    # --- Graph depth stats (from last index output) ---
    # The [Done] line has: "Done — N files, N symbols, N edges"
    local done_line
    done_line=$(echo "$last_index_output" | grep 'Done' | tail -1)
    local symbol_count edge_count file_count
    file_count=$(echo "$done_line" | grep -oE '[0-9]+ file' | grep -oE '[0-9]+' || echo 0)
    symbol_count=$(echo "$done_line" | grep -oE '[0-9]+ symbol' | grep -oE '[0-9]+' || echo 0)
    edge_count=$(echo "$done_line" | grep -oE '[0-9]+ edge' | grep -oE '[0-9]+' || echo 0)
    info "    graph: ${file_count} files, ${symbol_count} symbols, ${edge_count} edges"

    # --- Incremental indexing (modify one file, re-index) ---
    local incremental_times=()
    local touch_file
    touch_file=$(cd "$repo_path" && git ls-files 2>/dev/null | head -20 | tail -1 || true)
    for ((i = 1; i <= NUM_RUNS; i++)); do
        touch "$repo_path/$touch_file"
        local ms
        ms=$(time_ms "$nw" index --db "$db" --repo "$repo_path")
        incremental_times+=("$ms")
        info "    incremental run $i: ${ms}ms"
    done
    local incremental_median
    incremental_median=$(median "${incremental_times[@]}")
    info "    incremental median: ${incremental_median}ms"

    # --- Index size on disk ---
    local index_size_bytes
    index_size_bytes=$(find "$db_dir" -type f -exec stat -f%z {} + 2>/dev/null | awk '{s+=$1}END{print s+0}')

    # Warm up daemon for queries. The index and incremental steps
    # auto-start a daemon. Wait until it responds to queries.
    info "    warming up daemon..."
    local warmup_ok=0
    for ((w = 0; w < 15; w++)); do
        if "$nw" search --db "$db" --json "test" 2>/dev/null | python3 -c "import json,sys; json.load(sys.stdin)" 2>/dev/null; then
            warmup_ok=1
            break
        fi
        sleep 2
    done
    if [[ $warmup_ok -eq 0 ]]; then
        warn "    daemon warm-up failed — queries may be slow"
    fi
    # Additional warm-up passes
    for ((w = 0; w < 3; w++)); do
        "$nw" search --db "$db" --json "warmup" >/dev/null 2>&1 || true
        "$nw" context --db "$db" --json "warmup" >/dev/null 2>&1 || true
    done
    info "    daemon ready"

    # --- Queries (through daemon — representative of real usage) ---
    local all_latencies=()
    local query_results=()

    # Search queries
    while IFS= read -r query; do
        local latencies=()
        local result_count=0

        for ((i = 1; i <= NUM_RUNS; i++)); do
            local ms
            ms=$(time_ms_capture "$nw" search --db "$db" --json "$query")
            latencies+=("$ms")

            read_captured
            if [[ $i -eq 1 ]] && [[ -n "$CAPTURED_OUTPUT" ]]; then
                result_count=$(echo "$CAPTURED_OUTPUT" | python3 -c "
import json, sys
try:
    d = json.load(sys.stdin)
    if isinstance(d, list):
        print(len(d))
    else:
        print(len(d.get('results', d.get('matches', []))))
except: print(0)
" 2>/dev/null || echo 0)
            fi
        done

        local lat_median
        lat_median=$(median "${latencies[@]}")
        all_latencies+=("${latencies[@]}")

        local q_json
        q_json=$(json_quote "$query")
        local runs_json
        runs_json=$(printf '%s,' "${latencies[@]}")
        runs_json="[${runs_json%,}]"

        query_results+=("{\"query\": $q_json, \"kind\": \"search\", \"latency_median_ms\": $lat_median, \"results\": $result_count, \"runs\": $runs_json}")
        info "    search '$query': ${lat_median}ms (results=$result_count)"
    done < <(load_queries "$name" "search")

    # Context queries
    while IFS= read -r query; do
        local latencies=()
        local seeds=0 connected=0 unique_files=0

        for ((i = 1; i <= NUM_RUNS; i++)); do
            local ms
            ms=$(time_ms_capture "$nw" context --db "$db" --json "$query")
            latencies+=("$ms")

            read_captured
            if [[ $i -eq 1 ]] && [[ -n "$CAPTURED_OUTPUT" ]]; then
                seeds=$(echo "$CAPTURED_OUTPUT" | python3 -c "
import json, sys
try:
    d = json.load(sys.stdin)
    print(len(d.get('seeds', [])))
except: print(0)
" 2>/dev/null || echo 0)
                connected=$(echo "$CAPTURED_OUTPUT" | python3 -c "
import json, sys
try:
    d = json.load(sys.stdin)
    print(len(d.get('connected', [])))
except: print(0)
" 2>/dev/null || echo 0)
                unique_files=$(echo "$CAPTURED_OUTPUT" | python3 -c "
import json, sys
try:
    d = json.load(sys.stdin)
    files = set()
    for n in d.get('connected', []) + d.get('seeds', []):
        fp = n.get('file_path', '')
        if fp: files.add(fp)
    print(len(files))
except: print(0)
" 2>/dev/null || echo 0)
            fi
        done

        local lat_median
        lat_median=$(median "${latencies[@]}")
        all_latencies+=("${latencies[@]}")

        local q_json
        q_json=$(json_quote "$query")
        local runs_json
        runs_json=$(printf '%s,' "${latencies[@]}")
        runs_json="[${runs_json%,}]"

        query_results+=("{\"query\": $q_json, \"kind\": \"context\", \"latency_median_ms\": $lat_median, \"seeds\": $seeds, \"connected\": $connected, \"unique_files\": $unique_files, \"runs\": $runs_json}")
        info "    context '$query': ${lat_median}ms (seeds=$seeds, connected=$connected, unique_files=$unique_files)"
    done < <(load_queries "$name" "context")

    # Stop all benchmark daemons (clean slate for next tool/repo)
    "$nw" daemon stop --db "$db" --quiet 2>/dev/null || true
    "$nw" daemon stop --quiet 2>/dev/null || true

    # Compute p50/p95 across all query latencies
    local p50 p95
    p50=$(percentile 50 "${all_latencies[@]}")
    p95=$(percentile 95 "${all_latencies[@]}")

    # Write result
    local queries_json
    queries_json=$(printf '%s,' "${query_results[@]}")
    queries_json="[${queries_json%,}]"
    local index_runs_json
    index_runs_json=$(printf '%s,' "${index_times[@]}")
    index_runs_json="[${index_runs_json%,}]"
    local incremental_runs_json
    incremental_runs_json=$(printf '%s,' "${incremental_times[@]}")
    incremental_runs_json="[${incremental_runs_json%,}]"

    python3 -c "
import json
result = {
    'tool': 'nestweaver',
    'repo': '$name',
    'index_median_ms': $index_median,
    'index_runs': $index_runs_json,
    'incremental_median_ms': $incremental_median,
    'incremental_runs': $incremental_runs_json,
    'index_size_bytes': $index_size_bytes,
    'file_count': $file_count,
    'symbol_count': $symbol_count,
    'edge_count': $edge_count,
    'p50_ms': $p50,
    'p95_ms': $p95,
    'queries': $queries_json
}
with open('$result_file', 'w') as f:
    json.dump(result, f, indent=2)
"
    info "  [nestweaver] done — index=${index_median}ms incremental=${incremental_median}ms p50=${p50}ms p95=${p95}ms"
}

# ---------------------------------------------------------------------------
# benchmark_graphify REPO_NAME REPO_PATH
# Graphify (PyPI: graphifyy) uses `graphify update <path>` for AST-only
# indexing and `graphify query "<q>" --graph <path>/graphify-out/graph.json`.
# Output goes to <repo>/graphify-out/ by default.
# ---------------------------------------------------------------------------
benchmark_graphify() {
    local name="$1" repo_path="$2"
    local graph_file="$repo_path/graphify-out/graph.json"
    local result_file="$RESULTS_DIR/${name}-graphify.json"

    info "  [graphify] benchmarking $name..."

    local index_times=()
    for ((i = 1; i <= NUM_RUNS; i++)); do
        rm -rf "$repo_path/graphify-out"
        local ms
        ms=$(time_ms "$GRAPHIFY_BIN" update "$repo_path" --force --no-cluster)
        index_times+=("$ms")
        info "    index run $i: ${ms}ms"
    done
    local index_median
    index_median=$(median "${index_times[@]}")

    # Preserve graph.json for later graphify benchmark
    cp "$graph_file" "$RESULTS_DIR/${name}-graphify-graph.json" 2>/dev/null || true

    # --- Graph depth stats (parse graph.json) ---
    local symbol_count edge_count
    symbol_count=$(python3 -c "import json; d=json.load(open('$graph_file')); print(len(d.get('nodes',[])))" 2>/dev/null || echo 0)
    edge_count=$(python3 -c "import json; d=json.load(open('$graph_file')); print(len(d.get('links',d.get('edges',[]))))" 2>/dev/null || echo 0)
    info "    graph: ${symbol_count} nodes, ${edge_count} edges"

    # --- Index size on disk ---
    local index_size_bytes
    index_size_bytes=$(find "$repo_path/graphify-out" -type f -exec stat -f%z {} + 2>/dev/null | awk '{s+=$1}END{print s+0}')

    # Warm-up queries
    for ((w = 0; w < 3; w++)); do
        "$GRAPHIFY_BIN" query "warmup" --graph "$graph_file" >/dev/null 2>&1 || true
    done

    local all_latencies=()
    local query_results=()

    while IFS= read -r query; do
        local latencies=()
        local results=0 noise=0

        for ((i = 1; i <= NUM_RUNS; i++)); do
            local ms
            ms=$(time_ms_capture "$GRAPHIFY_BIN" query "$query" --graph "$graph_file")
            latencies+=("$ms")

            read_captured
            if [[ $i -eq 1 ]] && [[ -n "$CAPTURED_OUTPUT" ]]; then
                # Graphify outputs text — NODE lines are actual results
                results=$(echo "$CAPTURED_OUTPUT" | grep -c '^NODE ' || echo 0)
                noise=$(echo "$CAPTURED_OUTPUT" | grep '^NODE' | grep 'src= ' | wc -l | tr -d ' ')
            fi
        done

        local lat_median
        lat_median=$(median "${latencies[@]}")
        all_latencies+=("${latencies[@]}")

        local q_json runs_json
        q_json=$(json_quote "$query")
        runs_json=$(printf '%s,' "${latencies[@]}")
        runs_json="[${runs_json%,}]"

        query_results+=("{\"query\": $q_json, \"latency_median_ms\": $lat_median, \"results\": $results, \"noise_nodes\": $noise, \"runs\": $runs_json}")
        info "    query '$query': ${lat_median}ms (results=$results, noise=$noise)"
    done < <(load_queries "$name" "search"; load_queries "$name" "context")

    local p50 p95
    p50=$(percentile 50 "${all_latencies[@]}")
    p95=$(percentile 95 "${all_latencies[@]}")

    local queries_json index_runs_json
    queries_json=$(printf '%s,' "${query_results[@]}")
    queries_json="[${queries_json%,}]"
    index_runs_json=$(printf '%s,' "${index_times[@]}")
    index_runs_json="[${index_runs_json%,}]"

    python3 -c "
import json
result = {
    'tool': 'graphify',
    'repo': '$name',
    'index_median_ms': $index_median,
    'index_runs': $index_runs_json,
    'index_size_bytes': $index_size_bytes,
    'symbol_count': $symbol_count,
    'edge_count': $edge_count,
    'p50_ms': $p50,
    'p95_ms': $p95,
    'queries': $queries_json
}
with open('$result_file', 'w') as f:
    json.dump(result, f, indent=2)
"
    info "  [graphify] done — index=${index_median}ms p50=${p50}ms p95=${p95}ms"
}

# ---------------------------------------------------------------------------
# benchmark_gitnexus REPO_NAME REPO_PATH
# GitNexus stores its index inside the repo (.gitnexus/), so we operate
# from within the repo directory.
# ---------------------------------------------------------------------------
benchmark_gitnexus() {
    local name="$1" repo_path="$2"
    local result_file="$RESULTS_DIR/${name}-gitnexus.json"

    info "  [gitnexus] benchmarking $name..."

    local index_times=()
    for ((i = 1; i <= NUM_RUNS; i++)); do
        rm -rf "$repo_path/.gitnexus"
        local ms
        ms=$(time_ms "$GITNEXUS_BIN" analyze "$repo_path")
        index_times+=("$ms")
        info "    index run $i: ${ms}ms"
    done
    local index_median
    index_median=$(median "${index_times[@]}")

    # --- Graph depth stats (re-run analyze to capture output) ---
    local gn_stats
    gn_stats=$("$GITNEXUS_BIN" analyze "$repo_path" --force 2>&1 || true)
    local symbol_count edge_count
    symbol_count=$(echo "$gn_stats" | grep -oE '[0-9,]+ nodes' | tr -d ',' | grep -oE '[0-9]+' || echo 0)
    edge_count=$(echo "$gn_stats" | grep -oE '[0-9,]+ edges' | tr -d ',' | grep -oE '[0-9]+' || echo 0)
    info "    graph: ${symbol_count} nodes, ${edge_count} edges"

    # --- Index size on disk ---
    local index_size_bytes
    index_size_bytes=$(find "$repo_path/.gitnexus" -type f -exec stat -f%z {} + 2>/dev/null | awk '{s+=$1}END{print s+0}')

    # Warm-up queries
    for ((w = 0; w < 3; w++)); do
        "$GITNEXUS_BIN" query "warmup" -r "$repo_path" >/dev/null 2>&1 || true
    done

    local all_latencies=()
    local query_results=()

    while IFS= read -r query; do
        local latencies=()
        local results=0

        for ((i = 1; i <= NUM_RUNS; i++)); do
            local ms
            ms=$(time_ms_capture "$GITNEXUS_BIN" query "$query" -r "$repo_path")
            latencies+=("$ms")

            read_captured
            if [[ $i -eq 1 ]] && [[ -n "$CAPTURED_OUTPUT" ]]; then
                results=$(echo "$CAPTURED_OUTPUT" | python3 -c "
import json, sys
try:
    d = json.load(sys.stdin)
    procs = d.get('processes', [])
    print(len(procs))
except: print(0)
" 2>/dev/null || echo 0)
            fi
        done

        local lat_median
        lat_median=$(median "${latencies[@]}")
        all_latencies+=("${latencies[@]}")

        local q_json runs_json
        q_json=$(json_quote "$query")
        runs_json=$(printf '%s,' "${latencies[@]}")
        runs_json="[${runs_json%,}]"

        query_results+=("{\"query\": $q_json, \"latency_median_ms\": $lat_median, \"results\": $results, \"runs\": $runs_json}")
        info "    query '$query': ${lat_median}ms (results=$results)"
    done < <(load_queries "$name" "search"; load_queries "$name" "context")

    local p50 p95
    p50=$(percentile 50 "${all_latencies[@]}")
    p95=$(percentile 95 "${all_latencies[@]}")

    local queries_json index_runs_json
    queries_json=$(printf '%s,' "${query_results[@]}")
    queries_json="[${queries_json%,}]"
    index_runs_json=$(printf '%s,' "${index_times[@]}")
    index_runs_json="[${index_runs_json%,}]"

    python3 -c "
import json
result = {
    'tool': 'gitnexus',
    'repo': '$name',
    'index_median_ms': $index_median,
    'index_runs': $index_runs_json,
    'index_size_bytes': $index_size_bytes,
    'symbol_count': $symbol_count,
    'edge_count': $edge_count,
    'p50_ms': $p50,
    'p95_ms': $p95,
    'queries': $queries_json
}
with open('$result_file', 'w') as f:
    json.dump(result, f, indent=2)
"
    info "  [gitnexus] done — index=${index_median}ms p50=${p50}ms p95=${p95}ms"
}

