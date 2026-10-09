#!/usr/bin/env bash
# Check out the LadybugDB source the `lbug` crate builds from, and point
# LBUG_SOURCE_DIR at it.
#
# Remove once a published lbug crate builds from source. Until then the crate
# on crates.io cannot compile its own bundled sources, and lbug's build script
# uses LBUG_SOURCE_DIR instead.
#
# This file is the ONE place the tag and commit are pinned. CI calls it before
# every cargo build; for local development run
#   eval "$(scripts/fetch-lbug-source.sh)"
# or export LBUG_SOURCE_DIR to your own checkout of the same tag.
#
# Usage: scripts/fetch-lbug-source.sh [destination]
# The destination defaults to $RUNNER_TEMP/ladybug-<tag> on CI and to
# target/ladybug-<tag> otherwise. An existing checkout at the pinned commit is
# reused. Nothing else is accepted: a checkout at any other commit, or a
# Cargo.lock that pins a different lbug version, fails the script.
set -euo pipefail

LBUG_TAG="v0.21.2"
LBUG_COMMIT="c473940c7eafa27413264c907a5e781a8b3f0a4d"
LBUG_REPOSITORY="https://github.com/LadybugDB/ladybug.git"

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

# The checkout must be the source of the version Cargo.lock resolves, or the
# Rust bindings and the C++ library they wrap disagree.
locked="$(awk '/^name = "lbug"$/{getline; print $3}' "$root/Cargo.lock" | tr -d '"')"
if [[ "v${locked}" != "$LBUG_TAG" ]]; then
  echo "fetch-lbug-source: Cargo.lock pins lbug ${locked:-<none>}, but this script pins ${LBUG_TAG}; update both together" >&2
  exit 1
fi

if [[ $# -gt 0 ]]; then
  dest="$1"
elif [[ -n "${RUNNER_TEMP:-}" ]]; then
  dest="${RUNNER_TEMP}/ladybug-${LBUG_TAG}"
else
  dest="${root}/target/ladybug-${LBUG_TAG}"
fi

head_of() {
  git -C "$1" rev-parse --verify --quiet HEAD 2>/dev/null || true
}

if [[ -d "$dest/.git" && "$(head_of "$dest")" == "$LBUG_COMMIT" ]]; then
  echo "fetch-lbug-source: reusing ${dest} at ${LBUG_COMMIT}" >&2
else
  # An empty directory (say, one created with the right owner) is cloned into.
  if [[ -e "$dest" && -n "$(ls -A "$dest" 2>/dev/null)" ]]; then
    echo "fetch-lbug-source: ${dest} exists but is not ${LBUG_TAG} (${LBUG_COMMIT}); remove it or pass another destination" >&2
    exit 1
  fi
  mkdir -p "$(dirname "$dest")"
  # No submodules: the build needs none of them (benchmark, dataset,
  # extension), and a plain clone does not fetch them.
  git -c advice.detachedHead=false clone --quiet --depth 1 --branch "$LBUG_TAG" \
    "$LBUG_REPOSITORY" "$dest"
fi

actual="$(head_of "$dest")"
if [[ "$actual" != "$LBUG_COMMIT" ]]; then
  echo "fetch-lbug-source: ${LBUG_TAG} resolved to ${actual:-<nothing>}, expected ${LBUG_COMMIT}; refusing to build from it" >&2
  exit 1
fi

dest="$(cd "$dest" && pwd)"
if [[ -n "${GITHUB_ENV:-}" ]]; then
  echo "LBUG_SOURCE_DIR=${dest}" >>"$GITHUB_ENV"
fi
echo "fetch-lbug-source: LBUG_SOURCE_DIR=${dest}" >&2
printf 'export LBUG_SOURCE_DIR=%q\n' "$dest"
