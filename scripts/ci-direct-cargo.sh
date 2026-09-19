#!/usr/bin/env bash
# Internal CI Cargo adapter; the guard must run before argument transformation.
set -euo pipefail
# Runner-context checks prevent accidental local use; environment values are
# not proof of execution origin. Never assign CI markers to bypass this guard.
# BEGIN RUNNER PREDICATE
github_runner_context() {
  [ "$1" = true ] && [ -n "$2" ] && [ -n "$3" ] && [ -n "$4" ]
}
# END RUNNER PREDICATE
if ! github_runner_context "${GITHUB_ACTIONS:-}" "${RUNNER_TEMP:-}" "${RUNNER_OS:-}" "${GITHUB_RUN_ID:-}"; then
  echo 'mutation helpers require GitHub Actions runner context' >&2
  exit 1
fi
if [ -z "${NW_REAL_CARGO:-}" ] || [ ! -x "$NW_REAL_CARGO" ]; then
  echo 'NW_REAL_CARGO must identify an executable outside the shim' >&2; exit 1
fi
python3 - "$NW_REAL_CARGO" "$0" <<'PY'
import os
import sys
if os.path.samefile(sys.argv[1], sys.argv[2]):
    raise SystemExit('underlying Cargo resolves to the shim')
PY
# BEGIN ARGUMENT TRANSFORM (also exercised offline, without the guard or exec)
root=false
package_next=false
command="${1:-}"
for arg in "$@"; do
  [ "$arg" != -- ] || break
  if [ "$package_next" = true ]; then
    case "$arg" in nestweaver|nestweaver@*) root=true ;; esac
    package_next=false
  fi
  case "$arg" in
    --package|-p) package_next=true ;;
    --package=nestweaver|--package=nestweaver@*|-pnestweaver|-pnestweaver@*) root=true ;;
  esac
done
case "$command" in build|test|check|rustc|clippy) ;; *) root=false ;; esac
args=()
added=false
for arg in "$@"; do
  if [ "$arg" = -- ] && [ "$root" = true ]; then
    args+=(--features ci-direct-tests)
    root=false
    added=true
  fi
  args+=("$arg")
done
if [ "$root" = true ] && [ "$added" = false ]; then
  args+=(--features ci-direct-tests)
  added=true
fi
# END ARGUMENT TRANSFORM
if [ -n "${NW_CARGO_TRACE:-}" ]; then
  python3 - "$NW_CARGO_TRACE" "$NW_REAL_CARGO" "$added" "$$" "$PPID" "$#" "$@" "${args[@]}" <<'PY'
import hashlib
import json
import os
from pathlib import Path
import sys
import time
import uuid
sink, real, added, pid, parent, count, *argv = sys.argv[1:]
path = Path(real).resolve()
original = argv[:int(count)]
selected = []
for index, arg in enumerate(original):
    if arg == '--':
        break
    if arg in ('--package', '-p') and index + 1 < len(original):
        selected.append(original[index + 1])
    elif arg.startswith('--package='):
        selected.append(arg.split('=', 1)[1])
record = dict(event='start', invocation_id=str(uuid.uuid4()), monotonic_ns=time.monotonic_ns(),
              pid=int(pid), parent_pid=int(parent), cwd=os.getcwd(),
              selected_packages=selected, original_argv=original, effective_argv=argv[int(count):],
              cargo_path=str(path), cargo_sha256=hashlib.sha256(path.read_bytes()).hexdigest(),
              injected_feature=added == 'true')
# Start-only tracing keeps exec's process/signal/exit semantics unchanged.
fd = os.open(sink, os.O_WRONLY | os.O_APPEND | os.O_CREAT, 0o600)
try:
    os.write(fd, (json.dumps(record) + '\n').encode())
finally:
    os.close(fd)
PY
fi
exec "$NW_REAL_CARGO" "${args[@]}"
