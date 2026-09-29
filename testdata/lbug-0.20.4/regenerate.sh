#!/usr/bin/env bash
# Regenerate the LadybugDB 0.20.4 fixtures in this directory. See README.md.
#
# Usage: testdata/lbug-0.20.4/regenerate.sh [--out DIR]
#   --out DIR  write the compressed fixtures to DIR instead of replacing the
#              committed ones (for a dry run).
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo="$(cd "$here/../.." && pwd)"
dest="$here"
if [[ "${1:-}" == "--out" ]]; then
    dest="$(mkdir -p "$2" && cd "$2" && pwd)"
fi

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
out="$work/out"

# 1. Build and run the generator with the old engine. It checks its own
#    output (engine version 0.20.x, no engine-format marker) and exits
#    non-zero otherwise.
(
    # Pinned here AND in generator/.cargo/config.toml: without them the lbug
    # crate links the LATEST release library (silently writing 0.21-format
    # files) and caches a prebuilt library inside ~/.cargo/registry. Only for
    # this step: the current engine's build below keeps its own settings.
    export LBUG_VERSION=0.20.4
    export LBUG_BUILD_FROM_SOURCE=1
    unset LBUG_SOURCE_DIR LBUG_PRECOMPILED_LIBRARY_DIR LBUG_LIBRARY_DIR LBUG_SHARED
    cd "$here/generator"
    CARGO_TARGET_DIR="${GENERATOR_TARGET_DIR:-$work/target}" cargo run --quiet -- "$out"
)

# 2. Check the new files with the CURRENT engine: a primary-key lookup of the
#    non-ASCII key must miss while a scan finds it, the file must be refused,
#    and the pending log must be refused before replay.
(
    cd "$repo"
    NESTWEAVER_OLD_ENGINE_FIXTURE_DIR="$out" \
        cargo test --workspace --features ci-direct-tests --lib -- \
        engine_format_open_tests::a_pk_lookup_misses \
        engine_format_open_tests::a_database_the_old_engine_wrote \
        engine_format_open_tests::an_old_engine_log_is_refused
)

# 3. Only now replace the fixtures.
for file in pre-cutover.lbug pre-cutover-wal.lbug pre-cutover-wal.lbug.wal; do
    zstd -19 -q -f "$out/$file" -o "$dest/$file.zst"
done
echo "fixtures written to $dest"
