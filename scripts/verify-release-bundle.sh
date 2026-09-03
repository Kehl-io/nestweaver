#!/usr/bin/env bash

# Verify the complete, checksum-bound NestWeaver release bundle before a draft
# release can become public. This intentionally accepts only the four supported
# targets and their matching checksum files; an extra, duplicate, or missing
# consumer artifact is a release failure.

set -euo pipefail

readonly TARGETS=(
  aarch64-apple-darwin
  aarch64-unknown-linux-gnu
  x86_64-apple-darwin
  x86_64-unknown-linux-gnu
)

verify_bundle() {
  local bundle_dir=$1
  local tag=$2
  local expected_file actual_file target archive checksum referenced

  if [[ ! -d "$bundle_dir" ]]; then
    echo "release bundle directory does not exist: $bundle_dir" >&2
    return 1
  fi
  if [[ -z "$tag" || "$tag" == */* ]]; then
    echo "release tag must be non-empty and may not contain '/': $tag" >&2
    return 1
  fi

  expected_file=$(mktemp)
  actual_file=$(mktemp)

  for target in "${TARGETS[@]}"; do
    archive="nestweaver-${tag}-${target}.tar.gz"
    printf '%s\n%s\n' "$archive" "$archive.sha256" >> "$expected_file"
  done
  sort -o "$expected_file" "$expected_file"

  find "$bundle_dir" -maxdepth 1 -type f -printf '%f\n' | sort > "$actual_file"
  if ! diff -u "$expected_file" "$actual_file"; then
    echo "release bundle must contain exactly four archives and four checksums" >&2
    rm -f "$expected_file" "$actual_file"
    return 1
  fi

  for target in "${TARGETS[@]}"; do
    archive="nestweaver-${tag}-${target}.tar.gz"
    checksum="$archive.sha256"
    referenced=$(awk 'NF { print $2 }' "$bundle_dir/$checksum")
    if [[ "$referenced" != "$archive" ]]; then
      echo "$checksum must reference only $archive, found: ${referenced:-<empty>}" >&2
      rm -f "$expected_file" "$actual_file"
      return 1
    fi
    if ! (cd "$bundle_dir" && shasum -a 256 -c "$checksum"); then
      rm -f "$expected_file" "$actual_file"
      return 1
    fi
  done

  rm -f "$expected_file" "$actual_file"
}

self_test() {
  local fixture tag missing checksum extra
  fixture=$(mktemp -d)
  # Expand the path while the local exists; an EXIT trap that references the
  # local by name trips `set -u` after this function returns.
  # Expansion here is the safety property described above.
  # shellcheck disable=SC2064
  trap "rm -rf -- $(printf '%q' "$fixture")" EXIT
  tag=v0.0.0-policy-test

  for target in "${TARGETS[@]}"; do
    printf 'archive for %s\n' "$target" > "$fixture/nestweaver-${tag}-${target}.tar.gz"
    (
      cd "$fixture"
      shasum -a 256 "nestweaver-${tag}-${target}.tar.gz" \
        > "nestweaver-${tag}-${target}.tar.gz.sha256"
    )
  done

  verify_bundle "$fixture" "$tag"

  missing="$fixture/nestweaver-${tag}-aarch64-apple-darwin.tar.gz"
  mv "$missing" "$missing.omitted"
  if verify_bundle "$fixture" "$tag" >/dev/null 2>&1; then
    echo "self-test failed: a missing target was accepted" >&2
    return 1
  fi
  mv "$missing.omitted" "$missing"

  checksum="$fixture/nestweaver-${tag}-aarch64-unknown-linux-gnu.tar.gz.sha256"
  printf '%064d  %s\n' 0 "nestweaver-${tag}-aarch64-unknown-linux-gnu.tar.gz" > "$checksum"
  if verify_bundle "$fixture" "$tag" >/dev/null 2>&1; then
    echo "self-test failed: a corrupt checksum was accepted" >&2
    return 1
  fi
  (
    cd "$fixture"
    shasum -a 256 "nestweaver-${tag}-aarch64-unknown-linux-gnu.tar.gz" > "${checksum##*/}"
  )

  extra="$fixture/nestweaver-${tag}-unsupported.tar.gz"
  : > "$extra"
  if verify_bundle "$fixture" "$tag" >/dev/null 2>&1; then
    echo "self-test failed: an extra target was accepted" >&2
    return 1
  fi
  rm -f "$extra"

  verify_bundle "$fixture" "$tag" >/dev/null
  rm -rf "$fixture"
  trap - EXIT
  echo "release bundle verifier self-test passed"
}

case "${1:-}" in
  --self-test)
    self_test
    ;;
  "")
    echo "usage: $0 <bundle-dir> <tag> | --self-test" >&2
    exit 64
    ;;
  *)
    if [[ $# -ne 2 ]]; then
      echo "usage: $0 <bundle-dir> <tag> | --self-test" >&2
      exit 64
    fi
    verify_bundle "$1" "$2"
    ;;
esac
