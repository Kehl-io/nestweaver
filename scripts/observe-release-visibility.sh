#!/usr/bin/env bash

# Observe, rather than infer, whether an exact tag, GitHub release (including a
# private draft), or npm version exists. The dry-run gate captures this JSON
# before and after deliberately incomplete matrices.

set -euo pipefail

if [[ $# -ne 4 ]]; then
  echo "usage: $0 <owner/repo> <tag> <npm-package> <npm-version>" >&2
  exit 64
fi

repo=$1
tag=$2
npm_package=$3
npm_version=$4

if [[ ! "$repo" =~ ^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$ ]]; then
  echo "invalid GitHub repository: $repo" >&2
  exit 64
fi
if [[ -z "${GH_TOKEN:-}" ]]; then
  echo "GH_TOKEN is required to observe private draft releases" >&2
  exit 64
fi

tmp_dir=$(mktemp -d)
trap 'rm -rf -- "$tmp_dir"' EXIT

encoded_tag=$(jq -rn --arg value "$tag" '$value | @uri')
tag_status=$(curl --silent --show-error \
  --output "$tmp_dir/tag.json" --write-out '%{http_code}' \
  -H "Authorization: Bearer $GH_TOKEN" \
  -H 'Accept: application/vnd.github+json' \
  -H 'X-GitHub-Api-Version: 2022-11-28' \
  "https://api.github.com/repos/$repo/git/ref/tags/$encoded_tag")
case "$tag_status" in
  200) tag_visible=true ;;
  404) tag_visible=false ;;
  *)
    echo "GitHub tag observation returned HTTP $tag_status" >&2
    cat "$tmp_dir/tag.json" >&2
    exit 1
    ;;
esac

releases_json=$(gh api --paginate --slurp "repos/$repo/releases?per_page=100")
release_counts=$(jq -c --arg tag "$tag" '
  [ .[][] | select(.tag_name == $tag) ] |
  {release_count: length,
   draft_count: ([.[] | select(.draft == true)] | length),
   public_count: ([.[] | select(.draft == false)] | length)}
' <<< "$releases_json")

encoded_package=$(jq -rn --arg value "$npm_package" '$value | @uri')
encoded_version=$(jq -rn --arg value "$npm_version" '$value | @uri')
npm_status=$(curl --silent --show-error \
  --output "$tmp_dir/npm.json" --write-out '%{http_code}' \
  -H 'Accept: application/json' \
  "https://registry.npmjs.org/$encoded_package/$encoded_version")
case "$npm_status" in
  200) npm_visible=true ;;
  404) npm_visible=false ;;
  *)
    echo "npm visibility observation returned HTTP $npm_status" >&2
    cat "$tmp_dir/npm.json" >&2
    exit 1
    ;;
esac

jq -n \
  --arg tag "$tag" \
  --arg npm_package "$npm_package" \
  --arg npm_version "$npm_version" \
  --argjson tag_visible "$tag_visible" \
  --argjson npm_visible "$npm_visible" \
  --argjson release_counts "$release_counts" \
  '{tag: $tag, tag_visible: $tag_visible,
    release_count: $release_counts.release_count,
    draft_count: $release_counts.draft_count,
    public_count: $release_counts.public_count,
    npm_package: $npm_package, npm_version: $npm_version,
    npm_visible: $npm_visible,
    all_absent: (($tag_visible | not) and
      $release_counts.release_count == 0 and ($npm_visible | not))}'
