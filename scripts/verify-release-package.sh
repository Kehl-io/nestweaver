#!/usr/bin/env bash

# Validate the local npm wrapper and its version contract before either the
# GitHub release or npm package becomes public. The JSON emitted on stdout is
# consumed by the release workflow; diagnostics go to stderr.

set -euo pipefail

verify_release_package() {
  local repo_root=$1
  local release_tag=$2
  local package_dir=$3
  local cargo_version manifest_version npm_name npm_version pack_json
  local expected_files actual_files package_filename package_path package_sha256

  if [[ ! -d "$repo_root/npm" || ! -f "$repo_root/Cargo.toml" ]]; then
    echo "repository root is missing Cargo.toml or npm/: $repo_root" >&2
    return 1
  fi
  mkdir -p -- "$package_dir"
  package_dir=$(cd "$package_dir" && pwd)
  if [[ ! "$release_tag" =~ ^v[0-9]+\.[0-9]+\.[0-9]+([-.][0-9A-Za-z.-]+)?$ ]]; then
    echo "release tag is not a supported semantic version tag: $release_tag" >&2
    return 1
  fi

  cargo_version=$(
    awk '
      /^\[workspace\.package\]$/ { in_package = 1; next }
      /^\[/ { in_package = 0 }
      in_package && /^version[[:space:]]*=/ {
        value = $0
        sub(/^[^=]*=[[:space:]]*"/, "", value)
        sub(/"[[:space:]]*$/, "", value)
        print value
        exit
      }
    ' "$repo_root/Cargo.toml"
  )
  manifest_version=$(jq -er '.["."]' "$repo_root/.release-please-manifest.json")
  npm_name=$(jq -er '.name' "$repo_root/npm/package.json")
  npm_version=$(jq -er '.version' "$repo_root/npm/package.json")

  if [[ -z "$cargo_version" ]]; then
    echo "Cargo.toml has no workspace.package version" >&2
    return 1
  fi
  if [[ "$npm_name" != nestweaver ]]; then
    echo "npm package name is $npm_name, expected nestweaver" >&2
    return 1
  fi
  if [[ "$cargo_version" != "$npm_version" || "$manifest_version" != "$npm_version" ]]; then
    echo "release versions disagree: Cargo=$cargo_version manifest=$manifest_version npm=$npm_version" >&2
    return 1
  fi
  if [[ "$release_tag" != "v$npm_version" ]]; then
    echo "release tag $release_tag does not match package version v$npm_version" >&2
    return 1
  fi

  # nw-433: `libc` is deliberately ABSENT, not `["glibc"]`. npm's `libc` field
  # constrains the WHOLE package, and this package legitimately supports
  # macOS (no libc concept at all) as well as glibc Linux -- there is no way
  # to declare "glibc-only on Linux, unconstrained elsewhere" in that one
  # field. Declaring `["glibc"]` refused every macOS install with EBADPLATFORM
  # ("Actual libc: undefined"), and THIS assertion is what would silently
  # re-ship that regression: it required the very value that broke every
  # macOS install, so a release gate built to catch defects was instead
  # enforcing one. The musl-vs-glibc distinction that field was standing in
  # for is now checked in code (`isMuslLinux` in install.js), where "Linux,
  # and only Linux" is actually expressible.
  jq -e '
    .bin.nestweaver == "bin/nestweaver" and
    .scripts.postinstall == "node install.js" and
    (.files | sort) == ["README.md", "bin/", "install.js"] and
    (.os | sort) == ["darwin", "linux"] and
    (.cpu | sort) == ["arm64", "x64"] and
    (has("libc") | not)
  ' "$repo_root/npm/package.json" >/dev/null

  node "$(dirname "$0")/verify-release-install.js" "$repo_root/npm" >&2

  # Build the one immutable tarball that will be attested and published. A
  # dry-run proves only metadata for bytes npm may repack differently later.
  pack_json=$(cd "$repo_root/npm" && npm pack --ignore-scripts --json \
    --pack-destination "$package_dir")
  jq -e --arg version "$npm_version" '
    length == 1 and
    .[0].name == "nestweaver" and
    .[0].version == $version and
    (.[0].integrity | startswith("sha512-")) and
    ([.[0].files[].path] | sort) ==
      ["README.md", "bin/nestweaver", "install.js", "package.json"] and
    any(.[0].files[]; .path == "bin/nestweaver" and .mode == 493)
  ' <<< "$pack_json" >/dev/null

  expected_files=$(mktemp)
  actual_files=$(mktemp)
  printf '%s\n' README.md bin/nestweaver install.js package.json | sort > "$expected_files"
  jq -r '.[0].files[].path' <<< "$pack_json" | sort > "$actual_files"
  if ! diff -u "$expected_files" "$actual_files" >&2; then
    rm -f -- "$expected_files" "$actual_files"
    echo "npm package contents differ from the exact release wrapper inventory" >&2
    return 1
  fi
  rm -f -- "$expected_files" "$actual_files"

  package_filename=$(jq -er '.[0].filename' <<< "$pack_json")
  package_path="$package_dir/$package_filename"
  if [[ ! -f "$package_path" ]]; then
    echo "npm pack reported $package_filename but did not create it" >&2
    return 1
  fi
  package_sha256=$(sha256sum "$package_path" | awk '{print $1}')
  [[ "$package_sha256" =~ ^[0-9a-f]{64}$ ]]
  printf '%s  %s\n' "$package_sha256" "$package_filename" \
    > "$package_path.sha256"

  jq -n \
    --arg name "$npm_name" \
    --arg version "$npm_version" \
    --arg tag "$release_tag" \
    --arg integrity "$(jq -er '.[0].integrity' <<< "$pack_json")" \
    --arg filename "$package_filename" \
    --arg sha256 "$package_sha256" \
    '{name: $name, version: $version, tag: $tag,
      integrity: $integrity, filename: $filename, sha256: $sha256}'
}

verify_release_package_artifact() {
  local repo_root=$1
  local release_tag=$2
  local package_dir=$3
  local manifest="$package_dir/release-package.json"
  local package_filename package_path package_sha256 package_integrity
  local cargo_version manifest_version package_version extract_dir
  local expected_files actual_files
  local -a package_files

  if [[ ! -d "$repo_root/npm" || ! -f "$repo_root/Cargo.toml" ]]; then
    echo "repository root is missing Cargo.toml or npm/: $repo_root" >&2
    return 1
  fi
  if [[ ! "$release_tag" =~ ^v[0-9]+\.[0-9]+\.[0-9]+([-.][0-9A-Za-z.-]+)?$ ]]; then
    echo "release tag is not a supported semantic version tag: $release_tag" >&2
    return 1
  fi
  if [[ ! -f "$manifest" ]]; then
    echo "release package artifact has no release-package.json" >&2
    return 1
  fi

  mapfile -t package_files < <(
    find "$package_dir" -maxdepth 1 -type f -name '*.tgz' -printf '%f\n' | sort
  )
  if [[ ${#package_files[@]} -ne 1 ]]; then
    echo "release package artifact must contain exactly one npm tarball" >&2
    return 1
  fi
  package_filename=${package_files[0]}
  [[ "$package_filename" =~ ^nestweaver-[0-9]+\.[0-9]+\.[0-9]+([-.][0-9A-Za-z.-]+)?\.tgz$ ]] || {
    echo "unexpected npm package filename: $package_filename" >&2
    return 1
  }
  package_path="$package_dir/$package_filename"
  if [[ ! -f "$package_path.sha256" ]]; then
    echo "release package artifact has no checksum for $package_filename" >&2
    return 1
  fi

  expected_files=$(mktemp)
  actual_files=$(mktemp)
  printf '%s\n' \
    "$package_filename" "$package_filename.sha256" release-package.json \
    | sort > "$expected_files"
  find "$package_dir" -maxdepth 1 -type f -printf '%f\n' | sort > "$actual_files"
  if ! diff -u "$expected_files" "$actual_files" >&2; then
    rm -f -- "$expected_files" "$actual_files"
    echo "release package artifact inventory is not exact" >&2
    return 1
  fi
  rm -f -- "$expected_files" "$actual_files"

  (cd "$package_dir" && sha256sum -c -- "$package_filename.sha256") >&2
  package_sha256=$(sha256sum "$package_path" | awk '{print $1}')
  package_integrity=$(node -e '
    const crypto = require("crypto");
    const fs = require("fs");
    const bytes = fs.readFileSync(process.argv[1]);
    process.stdout.write("sha512-" + crypto.createHash("sha512").update(bytes).digest("base64"));
  ' "$package_path")
  package_version=$(jq -er '.version' "$repo_root/npm/package.json")
  cargo_version=$(
    awk '
      /^\[workspace\.package\]$/ { in_package = 1; next }
      /^\[/ { in_package = 0 }
      in_package && /^version[[:space:]]*=/ {
        value = $0
        sub(/^[^=]*=[[:space:]]*"/, "", value)
        sub(/"[[:space:]]*$/, "", value)
        print value
        exit
      }
    ' "$repo_root/Cargo.toml"
  )
  manifest_version=$(jq -er '.["."]' "$repo_root/.release-please-manifest.json")
  [[ "$package_version" = "$cargo_version" && "$package_version" = "$manifest_version" ]] || {
    echo "release versions disagree: Cargo=$cargo_version manifest=$manifest_version npm=$package_version" >&2
    return 1
  }
  [[ "$release_tag" = "v$package_version" ]] || {
    echo "release tag $release_tag does not match package version v$package_version" >&2
    return 1
  }
  jq -e \
    --arg name nestweaver \
    --arg version "$package_version" \
    --arg tag "$release_tag" \
    --arg integrity "$package_integrity" \
    --arg filename "$package_filename" \
    --arg sha256 "$package_sha256" '
      .name == $name and .version == $version and .tag == $tag and
      .integrity == $integrity and .filename == $filename and
      .sha256 == $sha256
    ' "$manifest" >/dev/null

  expected_files=$(mktemp)
  actual_files=$(mktemp)
  printf '%s\n' \
    package/README.md package/bin/nestweaver package/install.js package/package.json \
    | sort > "$expected_files"
  tar -tzf "$package_path" | sort > "$actual_files"
  if ! diff -u "$expected_files" "$actual_files" >&2; then
    rm -f -- "$expected_files" "$actual_files"
    echo "npm tarball inventory is not exact" >&2
    return 1
  fi
  rm -f -- "$expected_files" "$actual_files"

  extract_dir=$(mktemp -d)
  tar -xzf "$package_path" -C "$extract_dir"
  test -x "$extract_dir/package/bin/nestweaver"
  cmp "$repo_root/npm/README.md" "$extract_dir/package/README.md"
  cmp "$repo_root/npm/bin/nestweaver" "$extract_dir/package/bin/nestweaver"
  cmp "$repo_root/npm/install.js" "$extract_dir/package/install.js"
  cmp "$repo_root/npm/package.json" "$extract_dir/package/package.json"
  node "$(dirname "$0")/verify-release-install.js" "$extract_dir/package" >&2
  rm -rf -- "$extract_dir"

  jq -c . "$manifest"
}

case "${1:-}" in
  --self-test)
    repo_root=${2:-$(cd "$(dirname "$0")/.." && pwd)}
    version=$(jq -er '.version' "$repo_root/npm/package.json")
    package_dir=$(mktemp -d)
    trap 'rm -rf -- "$package_dir"' EXIT
    verify_release_package "$repo_root" "v$version" "$package_dir" \
      > "$package_dir/release-package.json"
    verify_release_package_artifact \
      "$repo_root" "v$version" "$package_dir" >/dev/null
    echo "release package verifier self-test passed"
    ;;
  "")
    echo "usage: $0 <repo-root> <release-tag> <package-dir> | --verify-artifact <repo-root> <release-tag> <package-dir> | --self-test [repo-root]" >&2
    exit 64
    ;;
  --verify-artifact)
    if [[ $# -ne 4 ]]; then
      echo "usage: $0 --verify-artifact <repo-root> <release-tag> <package-dir>" >&2
      exit 64
    fi
    verify_release_package_artifact "$2" "$3" "$4"
    ;;
  *)
    if [[ $# -ne 3 ]]; then
      echo "usage: $0 <repo-root> <release-tag> <package-dir> | --verify-artifact <repo-root> <release-tag> <package-dir> | --self-test [repo-root]" >&2
      exit 64
    fi
    verify_release_package "$1" "$2" "$3"
    ;;
esac
