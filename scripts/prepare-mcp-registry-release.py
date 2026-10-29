#!/usr/bin/env python3
"""Read-only preparation/verification for manual MCP Registry publication.

Never requests OIDC, executes release code, publishes npm, or submits a listing.
Only the fixed public GitHub repository, npm registry, and MCP Registry are read.
"""
import argparse
import base64
import gzip
import hashlib
import io
import json
from pathlib import Path
import re
import subprocess
import tarfile
import urllib.error
import urllib.request

REPOSITORY = 'Kehl-io/nestweaver'
NAME = 'io.github.Kehl-io/nestweaver'
REGISTRY = 'https://registry.modelcontextprotocol.io'
PLATFORMS = ('darwin-arm64', 'darwin-x64', 'linux-arm64', 'linux-x64')
VERSION = re.compile(r'(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\Z')
SHA = re.compile(r'[0-9a-f]{40}\Z')


def require(condition, message):
    if not condition:
        raise ValueError(message)


def validate_inputs(version, commit):
    require(bool(VERSION.fullmatch(version)), 'version must be an exact stable X.Y.Z')
    require(bool(SHA.fullmatch(commit)), 'expected SHA must be 40 lowercase hex characters')


def github(path):
    result = subprocess.run(['gh', 'api', f'repos/{REPOSITORY}/{path}'],
                            check=True, capture_output=True, text=True, timeout=30)
    return json.loads(result.stdout)


def read_url(url, missing=False, limit=2 * 1024 * 1024):
    # No credentials, token headers, or caller-provided hosts are accepted here.
    try:
        with urllib.request.urlopen(url, timeout=30) as response:
            data = response.read(limit + 1)
    except urllib.error.HTTPError as error:
        if missing and error.code == 404:
            return None
        raise ValueError(f'public validation request failed: HTTP {error.code}') from error
    require(len(data) <= limit, 'public validation response exceeds size limit')
    return data


def public_json(url, missing=False):
    data = read_url(url, missing=missing)
    return None if data is None else json.loads(data)


def release_commit(version, expected, fetch=github):
    obj = fetch(f'git/ref/tags/v{version}')['object']
    for _ in range(5):
        require(bool(SHA.fullmatch(obj.get('sha', ''))), 'invalid tag object SHA')
        if obj.get('type') == 'commit':
            require(obj['sha'] == expected, 'release tag does not resolve to expected SHA')
            break
        require(obj.get('type') == 'tag', 'release tag does not point to a commit')
        obj = fetch(f"git/tags/{obj['sha']}")['object']
    else:
        raise ValueError('release tag chain too deep')
    release = fetch(f'releases/tags/v{version}')
    require(release.get('tag_name') == f'v{version}' and release.get('draft') is False
            and release.get('prerelease') is False and bool(release.get('published_at')),
            'release must be a published stable GitHub release')


def source_file(path, commit, fetch=github):
    payload = fetch(f'contents/{path}?ref={commit}')
    require(payload.get('type') == 'file' and payload.get('encoding') == 'base64',
            'expected a GitHub source file')
    data = base64.b64decode(''.join(payload['content'].split()), validate=True)
    require(len(data) <= 256 * 1024, 'source metadata exceeds size limit')
    return data


def validate_manifest(manifest, package, version):
    require(manifest.get('name') == NAME and manifest.get('version') == version,
            'wrong registry identity or version')
    require(manifest.get('repository') == {'url': f'https://github.com/{REPOSITORY}', 'source': 'github'},
            'wrong source repository')
    require(package.get('name') == 'nestweaver' and package.get('version') == version
            and package.get('mcpName') == NAME, 'wrong npm ownership metadata')
    require(package.get('optionalDependencies') == {f'nestweaver-{p}': version for p in PLATFORMS},
            'platform package versions must match the release')
    entries = manifest.get('packages', [])
    require(len(entries) == 1, 'exactly one npm registry package is expected')
    entry = entries[0]
    require(entry.get('registryType') == 'npm' and entry.get('identifier') == 'nestweaver'
            and entry.get('version') == version and entry.get('runtimeHint') == 'npx'
            and entry.get('transport') == {'type': 'stdio'}, 'wrong npm launch identity')
    args = entry.get('packageArguments', [])
    require(len(args) == 2 and args[0] == {'type': 'positional', 'value': 'mcp'},
            'expected the mcp subcommand')
    db = args[1]
    require(db.get('type') == 'named' and db.get('name') == '--db'
            and db.get('format') == 'filepath' and db.get('isRequired') is True
            and 'default' not in db and 'value' not in db, 'database path must remain a required input')
    require(entry.get('registryBaseUrl', 'https://registry.npmjs.org') == 'https://registry.npmjs.org',
            'unexpected npm registry')
    require(set(db) <= {'type', 'name', 'description', 'format', 'isRequired', 'placeholder'},
            'unexpected database argument configuration')
    require(not manifest.get('remotes') and not entry.get('runtimeArguments')
            and not entry.get('environmentVariables'), 'unexpected launch configuration')


def validate_npm(package, version, fetch=public_json, download=read_url):
    for name in ('nestweaver', *(f'nestweaver-{p}' for p in PLATFORMS)):
        metadata = fetch(f'https://registry.npmjs.org/{name}/{version}')
        require(metadata.get('name') == name and metadata.get('version') == version,
                f'missing or mismatched npm package: {name}')
        dist = metadata.get('dist', {})
        integrity = dist.get('integrity', '')
        require(integrity.startswith('sha512-')
                and len(base64.b64decode(integrity[7:], validate=True)) == 64,
                f'missing SHA-512 npm integrity: {name}')
        if name != 'nestweaver':
            continue
        require(metadata.get('mcpName') == NAME, 'published npm mcpName does not match')
        url = f'https://registry.npmjs.org/nestweaver/-/nestweaver-{version}.tgz'
        require(dist.get('tarball') == url, 'unexpected npm wrapper tarball URL')
        data = download(url)
        require('sha512-' + base64.b64encode(hashlib.sha512(data).digest()).decode() == integrity,
                'npm wrapper tarball integrity mismatch')
        require(hashlib.sha1(data).hexdigest() == dist.get('shasum'), 'npm wrapper SHA-1 mismatch')
        # Bound decompression; never extract files or execute package scripts.
        with gzip.GzipFile(fileobj=io.BytesIO(data)) as archive:
            unpacked = archive.read(8 * 1024 * 1024 + 1)
        require(len(unpacked) <= 8 * 1024 * 1024, 'npm wrapper archive exceeds size limit')
        with tarfile.open(fileobj=io.BytesIO(unpacked), mode='r:') as archive:
            member = archive.getmember('package/package.json')
            require(member.isfile() and member.size <= 256 * 1024, 'invalid packed package.json')
            packed = json.load(archive.extractfile(member))
        require(packed == package, 'published wrapper package.json differs from release source')


def endpoint(version):
    return f'{REGISTRY}/v0.1/servers/io.github.Kehl-io%2Fnestweaver/versions/{version}'


def matches_registry(payload, manifest):
    actual = payload.get('server', {})
    require(all(actual.get(key) == value for key, value in manifest.items()),
            'existing registry entry differs from the release manifest')


def registry_state(manifest, fetch=public_json, require_published=False):
    exact = fetch(endpoint(manifest['version']), missing=True)
    latest = fetch(endpoint('latest'), missing=True)
    if exact is not None:
        matches_registry(exact, manifest)
        require(latest is not None, 'latest registry entry is missing')
        matches_registry(latest, manifest)
        return False
    require(not require_published, 'exact public registry version is still absent')
    if latest is not None:
        require(latest.get('server', {}).get('name') == NAME, 'wrong latest registry identity')
        latest_version = latest['server'].get('version', '')
        require(bool(VERSION.fullmatch(latest_version)), 'unexpected latest version; inspect manually')
        require(tuple(map(int, manifest['version'].split('.'))) > tuple(map(int, latest_version.split('.'))),
                'refusing to publish an older version than the current registry listing')
    return True


def prepare(version, commit, output, expected_manifest=None, fetch_github=github,
            fetch_public=public_json, download=read_url):
    validate_inputs(version, commit)
    release_commit(version, commit, fetch=fetch_github)
    data = source_file('server.json', commit, fetch=fetch_github)
    digest = hashlib.sha256(data).hexdigest()
    if expected_manifest is not None:
        require(digest == expected_manifest, 'manifest changed between preparation and publication')
    manifest = json.loads(data)
    package = json.loads(source_file('npm/package.json', commit, fetch=fetch_github))
    validate_manifest(manifest, package, version)
    validate_npm(package, version, fetch=fetch_public, download=download)
    should_publish = registry_state(manifest, fetch=fetch_public)
    output.mkdir(parents=True, exist_ok=True)
    (output / 'server.json').write_bytes(data)
    receipt = {'repository': REPOSITORY, 'commit': commit, 'version': version,
               'manifest_sha256': digest, 'should_publish': should_publish,
               'version_url': endpoint(version), 'latest_url': endpoint('latest')}
    (output / 'validation.json').write_text(json.dumps(receipt, indent=2) + '\n')
    return receipt


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--version', required=True)
    parser.add_argument('--expected-sha', required=True)
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--expected-manifest-sha256')
    parser.add_argument('--github-output', type=Path)
    parser.add_argument('--verify-published', action='store_true')
    args = parser.parse_args()
    if args.verify_published:
        validate_inputs(args.version, args.expected_sha)
        manifest = json.loads((args.output / 'server.json').read_text())
        require(manifest.get('version') == args.version and manifest.get('name') == NAME,
                'wrong prepared manifest')
        registry_state(manifest, require_published=True)
        print(json.dumps({'verified_version': endpoint(args.version), 'latest': endpoint('latest')}))
        return
    receipt = prepare(args.version, args.expected_sha, args.output, args.expected_manifest_sha256)
    if args.github_output:
        with args.github_output.open('a') as stream:
            stream.write(f"manifest_sha256={receipt['manifest_sha256']}\n")
            stream.write(f"should_publish={str(receipt['should_publish']).lower()}\n")
    print(json.dumps(receipt))


if __name__ == '__main__':
    main()
