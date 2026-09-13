#!/usr/bin/env bash
# Run only on an isolated worker, after compilation has finished. Preserve
# measured load and both raw runs; never translate a missing run into success.
set -euo pipefail
mkdir -p backlog-performance
git rev-parse HEAD > backlog-performance/commit.txt
rustc --version > backlog-performance/rustc.txt
uname -a > backlog-performance/kernel.txt
lscpu > backlog-performance/cpu.txt
free -b > backlog-performance/memory.txt
cp Cargo.lock backlog-performance/Cargo.lock
python3 - <<'PY' > backlog-performance/quiet-window.json
import json, time
from pathlib import Path
def counters():
    # guest values are already included in user/nice.
    parts = list(map(int, Path('/proc/stat').read_text().splitlines()[0].split()[1:9]))
    return sum(parts), parts[3] + parts[4]
samples = []
quiet = 0
before = counters()
for _ in range(24):
    time.sleep(5)
    after = counters()
    total = after[0] - before[0]
    busy = 1 - (after[1] - before[1]) / max(total, 1)
    samples.append({'busy_fraction': busy})
    quiet = quiet + 1 if busy < .10 else 0
    before = after
    if quiet >= 3:
        print(json.dumps({'quiet': True, 'samples': samples}))
        break
else:
    print(json.dumps({'quiet': False, 'samples': samples}))
    raise SystemExit('worker did not become quiet; no valid benchmark window')
PY
mapfile -t binaries < <(find target/release/deps -maxdepth 1 -type f -name 'remove_repo_benchmarks-*' -executable)
test "${#binaries[@]}" -eq 1
for run in 1 2; do
  /usr/bin/time -v env BENCH_HUB_DEGREES=1000,8700,86800 \
    "${binaries[0]}" --bench > "backlog-performance/remove-repo-${run}.log" 2>&1
  /usr/bin/time -v env RUST_LOG=info \
    target/release/examples/project_materialization_benchmark \
    > "backlog-performance/materialization-${run}.log" 2>&1
done
