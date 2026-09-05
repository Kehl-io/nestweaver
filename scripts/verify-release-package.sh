#!/usr/bin/env bash

# Validate the npm packages -- the main "nestweaver" wrapper and its four
# per-platform optionalDependencies -- before either the GitHub release or any
# npm package becomes public. The JSON emitted on stdout by the packing modes
# is consumed by the release workflow; diagnostics go to stderr.
#
# nw-425 / nw-433: the wrapper used to have a `postinstall` that downloaded
# the matching GitHub Release archive at install time. pnpm 10+ blocks
# lifecycle scripts by default, so `pnpm add nestweaver` silently produced a
# wrapper with no binary. There is now no lifecycle script anywhere: each
# supported platform ships as its own optionalDependency carrying the real
# prebuilt binary (the esbuild/@rollup/swc pattern), and `npm/bin/nestweaver`
# resolves and execs whichever one actually landed at INVOCATION time. This
# script verifies the five package.json contracts stay in lockstep and that
# each package's packed tarball actually contains an executable
# `bin/nestweaver`.

set -euo pipefail

PLATFORM_KEYS=(darwin-arm64 darwin-x64 linux-arm64 linux-x64)

platform_os() {
  case "$1" in
    darwin-*) echo darwin ;;
    linux-*) echo linux ;;
  esac
}

platform_cpu() {
  case "$1" in
    *-arm64) echo arm64 ;;
    *-x64) echo x64 ;;
  esac
}

release_versions() {
  local repo_root=$1
  local cargo_version manifest_version npm_version
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
  npm_version=$(jq -er '.version' "$repo_root/npm/package.json")
  if [[ -z "$cargo_version" ]]; then
    echo "Cargo.toml has no workspace.package version" >&2
    return 1
  fi
  if [[ "$cargo_version" != "$npm_version" || "$manifest_version" != "$npm_version" ]]; then
    echo "release versions disagree: Cargo=$cargo_version manifest=$manifest_version npm=$npm_version" >&2
    return 1
  fi
  echo "$npm_version"
}

# nw-433: `libc` is deliberately ABSENT from the MAIN package (it spans both
# macOS, where libc is not a concept, and glibc Linux) but is now safe and
# correct on the per-platform Linux packages, which are Linux-only. Declaring
# it on the whole package was what broke every macOS install with
# EBADPLATFORM ("Actual libc: undefined"); declaring it on a Linux-only
# package cannot recreate that, because there is no macOS package for it to
# apply to.
verify_main_contract() {
  local repo_root=$1 npm_version=$2
  local optional_json expected_optional
  optional_json=$(jq -c '.optionalDependencies // {}' "$repo_root/npm/package.json")

  jq -e --arg version "$npm_version" '
    .bin.nestweaver == "bin/nestweaver" and
    (.scripts // {} | length) == 0 and
    (.files | sort) == ["README.md", "bin/"] and
    (.os | sort) == ["darwin", "linux"] and
    (.cpu | sort) == ["arm64", "x64"] and
    (has("libc") | not) and
    (.version == $version)
  ' "$repo_root/npm/package.json" >/dev/null || {
    echo "npm/package.json failed the main-package contract" >&2
    return 1
  }

  expected_optional=$(
    printf '%s\n' "${PLATFORM_KEYS[@]}" |
      jq -R --arg version "$npm_version" '{(("nestweaver-" + .)): $version}' |
      jq -s 'add'
  )
  if [[ "$(jq -Sc . <<< "$optional_json")" != "$(jq -Sc . <<< "$expected_optional")" ]]; then
    echo "npm/package.json optionalDependencies do not exactly match the four platform packages at $npm_version" >&2
    echo "  got:      $optional_json" >&2
    echo "  expected: $expected_optional" >&2
    return 1
  fi
}

verify_platform_contract() {
  local repo_root=$1 platform_key=$2 npm_version=$3
  local pkg_json="$repo_root/npm/platforms/$platform_key/package.json"
  local expected_name="nestweaver-$platform_key"
  local expected_os expected_cpu expect_libc
  expected_os=$(platform_os "$platform_key")
  expected_cpu=$(platform_cpu "$platform_key")
  expect_libc=false
  [[ "$expected_os" == linux ]] && expect_libc=true

  if [[ ! -f "$pkg_json" ]]; then
    echo "missing platform package.json: $pkg_json" >&2
    return 1
  fi

  jq -e \
    --arg name "$expected_name" \
    --arg version "$npm_version" \
    --arg os "$expected_os" \
    --arg cpu "$expected_cpu" \
    --argjson expect_libc "$expect_libc" '
    .name == $name and
    .version == $version and
    (.os | sort) == [$os] and
    (.cpu | sort) == [$cpu] and
    (.files | sort) == ["bin/"] and
    (.scripts // {} | length) == 0 and
    (if $expect_libc then (.libc // []) == ["glibc"] else (has("libc") | not) end)
  ' "$pkg_json" >/dev/null || {
    echo "$pkg_json failed the platform-package contract (os=$expected_os cpu=$expected_cpu libc=$expect_libc)" >&2
    return 1
  }
}

# Pack whatever npm package sits in `pkg_dir` (main "npm/" or one of
# "npm/platforms/<key>") into `package_dir`, verifying it contains an
# executable `bin/nestweaver` and nothing outside the files it declares.
# Returns the same {name, version, tag, integrity, filename, sha256} shape
# regardless of which package this is, so the release workflow can call it
# identically for all five.
pack_package() {
  local pkg_dir=$1 release_tag=$2 package_dir=$3
  local pkg_name pkg_version pack_json declared_files package_filename
  local package_path package_sha256 actual_paths

  mkdir -p -- "$package_dir"
  package_dir=$(cd "$package_dir" && pwd)
  pkg_name=$(jq -er '.name' "$pkg_dir/package.json")
  pkg_version=$(jq -er '.version' "$pkg_dir/package.json")
  declared_files=$(jq -er '(.files // []) | sort | join(",")' "$pkg_dir/package.json")

  pack_json=$(cd "$pkg_dir" && npm pack --ignore-scripts --json --pack-destination "$package_dir")
  jq -e --arg version "$pkg_version" '
    length == 1 and .[0].name != "" and .[0].version == $version and
    (.[0].integrity | startswith("sha512-"))
  ' <<< "$pack_json" >/dev/null

  jq -e '
    any(.[0].files[]; .path == "bin/nestweaver" and .mode == 493)
  ' <<< "$pack_json" >/dev/null || {
    echo "$pkg_dir did not pack an executable bin/nestweaver" >&2
    return 1
  }

  # Every packed path must be either package.json (npm always includes it) or
  # fall under one of the directories this package's own `files` field
  # declares -- catches accidental inclusion of dev-only files (node_modules,
  # test fixtures, .git) that `files` should have excluded.
  actual_paths=$(jq -r '.[0].files[].path' <<< "$pack_json")
  while IFS= read -r p; do
    [[ "$p" == "package.json" ]] && continue
    IFS=',' read -r -a dirs <<< "$declared_files"
    matched=false
    for d in "${dirs[@]}"; do
      [[ "$p" == "$d"* ]] && matched=true && break
    done
    if [[ "$matched" != true ]]; then
      echo "$pkg_dir packed an undeclared path: $p (declared files: $declared_files)" >&2
      return 1
    fi
  done <<< "$actual_paths"

  package_filename=$(jq -er '.[0].filename' <<< "$pack_json")
  package_path="$package_dir/$package_filename"
  test -f "$package_path"
  package_sha256=$(sha256sum "$package_path" | awk '{print $1}')
  [[ "$package_sha256" =~ ^[0-9a-f]{64}$ ]]
  printf '%s  %s\n' "$package_sha256" "$package_filename" > "$package_path.sha256"

  jq -n \
    --arg name "$pkg_name" \
    --arg version "$pkg_version" \
    --arg tag "$release_tag" \
    --arg integrity "$(jq -er '.[0].integrity' <<< "$pack_json")" \
    --arg filename "$package_filename" \
    --arg sha256 "$package_sha256" \
    '{name: $name, version: $version, tag: $tag,
      integrity: $integrity, filename: $filename, sha256: $sha256}'
}

# Re-verify a downloaded single-package artifact (one .tgz + its .sha256 +
# release-package.json) before it is attested, published, or recovered.
# Generalized over `expected_name` so the same check applies to the main
# package and to each of the four platform packages.
verify_package_artifact() {
  local package_dir=$1 release_tag=$2 expected_name=$3
  local manifest="$package_dir/release-package.json"
  local package_filename package_path package_sha256 package_integrity
  local package_version expected_files actual_files
  local -a package_files

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
  package_version=$(jq -er '.version' "$manifest")
  [[ "$release_tag" == "v$package_version" || "$expected_name" != nestweaver ]] || {
    echo "release tag $release_tag does not match package version v$package_version" >&2
    return 1
  }

  jq -e \
    --arg name "$expected_name" \
    --arg version "$package_version" \
    --arg tag "$release_tag" \
    --arg integrity "$package_integrity" \
    --arg filename "$package_filename" \
    --arg sha256 "$package_sha256" '
      .name == $name and .version == $version and .tag == $tag and
      .integrity == $integrity and .filename == $filename and
      .sha256 == $sha256
    ' "$manifest" >/dev/null

  tar -tzf "$package_path" | grep -qFx package.json 2>/dev/null || {
    tar -tzf "$package_path" | grep -qx 'package/package\.json' || {
      echo "npm tarball is missing package.json" >&2
      return 1
    }
  }
  tar -tzf "$package_path" | grep -qx 'package/bin/nestweaver' || {
    echo "npm tarball is missing bin/nestweaver" >&2
    return 1
  }

  extract_dir=$(mktemp -d)
  tar -xzf "$package_path" -C "$extract_dir"
  test -x "$extract_dir/package/bin/nestweaver"
  rm -rf -- "$extract_dir"

  jq -c . "$manifest"
}

verify_release_package() {
  local repo_root=$1 release_tag=$2 package_dir=$3
  local npm_version key

  if [[ ! -d "$repo_root/npm" || ! -f "$repo_root/Cargo.toml" ]]; then
    echo "repository root is missing Cargo.toml or npm/: $repo_root" >&2
    return 1
  fi
  if [[ ! "$release_tag" =~ ^v[0-9]+\.[0-9]+\.[0-9]+([-.][0-9A-Za-z.-]+)?$ ]]; then
    echo "release tag is not a supported semantic version tag: $release_tag" >&2
    return 1
  fi

  npm_version=$(release_versions "$repo_root")
  if [[ "$release_tag" != "v$npm_version" ]]; then
    echo "release tag $release_tag does not match package version v$npm_version" >&2
    return 1
  fi

  verify_main_contract "$repo_root" "$npm_version"
  for key in "${PLATFORM_KEYS[@]}"; do
    verify_platform_contract "$repo_root" "$key" "$npm_version"
  done

  pack_package "$repo_root/npm" "$release_tag" "$package_dir"
}

case "${1:-}" in
  --self-test)
    repo_root=${2:-$(cd "$(dirname "$0")/.." && pwd)}
    version=$(jq -er '.version' "$repo_root/npm/package.json")
    package_dir=$(mktemp -d)
    trap 'rm -rf -- "$package_dir"' EXIT

    verify_release_package "$repo_root" "v$version" "$package_dir" \
      > "$package_dir/release-package.json"
    verify_package_artifact "$package_dir" "v$version" nestweaver >/dev/null

    # Platform packages ship real compiled binaries staged by the release
    # build matrix; there is no compiled binary here to pack all four for
    # real. Exercise the SAME packing/verification code path against one
    # representative platform package (darwin-arm64) with a placeholder
    # executable staged into a throwaway COPY -- never the tracked source
    # tree -- so the mechanics (mode-493 preservation, undeclared-path
    # rejection, artifact re-verification) are proven without needing a
    # cargo build.
    fixture_dir=$(mktemp -d)
    cp -R "$repo_root/npm/platforms/darwin-arm64/." "$fixture_dir/"
    mkdir -p "$fixture_dir/bin"
    printf '#!/bin/sh\necho fixture\n' > "$fixture_dir/bin/nestweaver"
    chmod 755 "$fixture_dir/bin/nestweaver"
    platform_package_dir=$(mktemp -d)
    pack_package "$fixture_dir" "v$version" "$platform_package_dir" \
      > "$platform_package_dir/release-package.json"
    verify_package_artifact "$platform_package_dir" "v$version" nestweaver-darwin-arm64 >/dev/null
    rm -rf -- "$fixture_dir" "$platform_package_dir"

    echo "release package verifier self-test passed"
    ;;
  "")
    echo "usage: $0 <repo-root> <release-tag> <package-dir> | --pack <pkg-dir> <release-tag> <package-dir> | --verify-artifact <package-dir> <release-tag> <expected-name> | --self-test [repo-root]" >&2
    exit 64
    ;;
  --pack)
    if [[ $# -ne 4 ]]; then
      echo "usage: $0 --pack <pkg-dir> <release-tag> <package-dir>" >&2
      exit 64
    fi
    pack_package "$2" "$3" "$4"
    ;;
  --verify-artifact)
    if [[ $# -ne 4 ]]; then
      echo "usage: $0 --verify-artifact <package-dir> <release-tag> <expected-name>" >&2
      exit 64
    fi
    verify_package_artifact "$2" "$3" "$4"
    ;;
  *)
    if [[ $# -ne 3 ]]; then
      echo "usage: $0 <repo-root> <release-tag> <package-dir> | --pack <pkg-dir> <release-tag> <package-dir> | --verify-artifact <package-dir> <release-tag> <expected-name> | --self-test [repo-root]" >&2
      exit 64
    fi
    verify_release_package "$1" "$2" "$3"
    ;;
esac
