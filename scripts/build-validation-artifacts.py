#!/usr/bin/env python3
"""One Cargo compilation; retain executable artifacts for later validation without Cargo.

The caller builds frontend assets first. This does not execute tests. The resulting
manifest binds every executable to the source commit and records runtime settings
normally supplied by Cargo. Do not use CI-only or fixture-bypass features locally.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import subprocess
import sys
import tomllib

ROOT = Path(__file__).resolve().parents[1]


def digest(path):
    value = hashlib.sha256()
    with Path(path).open('rb') as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b''):
            value.update(chunk)
    return value.hexdigest()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--output', required=True, type=Path)
    parser.add_argument('--release', action='store_true')
    args = parser.parse_args()
    args.output = args.output.resolve()
    args.output.mkdir(parents=True, exist_ok=True)
    if (args.output / 'cargo.jsonl').exists():
        parser.error('output already contains a build attempt; choose a fresh evidence directory')
    metadata = json.loads(subprocess.check_output(
        ['cargo', 'metadata', '--no-deps', '--format-version=1', '--locked'], cwd=ROOT))
    members = set(metadata['workspace_members'])
    features = []
    for package in metadata['packages']:
        if package['id'] not in members:
            continue
        for feature in package['features']:
            if feature in ('ci-direct-tests', 'release-fixture-hooks'):
                continue
            if feature == 'metal' and platform.system() != 'Darwin':
                continue
            features.append(f"{package['name']}/{feature}")
    command = ['cargo', 'test', '--workspace', '--lib', '--bins', '--tests', '--no-run',
               '--locked', '--message-format=json', '--features', ','.join(sorted(features))]
    if args.release:
        command.append('--release')
    environment = os.environ.copy()
    environment.setdefault('CARGO_BUILD_JOBS', '2')
    environment.setdefault('CMAKE_BUILD_PARALLEL_LEVEL', '2')
    # Keep the native engine and all Rust test artifacts in one bounded profile.
    if not args.release:
        environment.setdefault('CARGO_PROFILE_DEV_DEBUG', '0')
    commit = subprocess.check_output(['git', 'rev-parse', 'HEAD'], cwd=ROOT, text=True).strip()
    dirty = subprocess.check_output(['git', 'status', '--porcelain', '--untracked-files=all'], cwd=ROOT, text=True)
    if dirty:
        parser.error('commit the reviewed complete tree before building')
    (args.output / 'build-input.json').write_text(json.dumps(dict(
        commit=commit, command=command, features=sorted(features), platform=platform.platform(),
        script_sha256=digest(__file__)), indent=2) + '\n')
    artifacts = {}
    with (args.output / 'cargo.jsonl').open('w') as log:
        process = subprocess.Popen(command, cwd=ROOT, env=environment, stdout=subprocess.PIPE, text=True)
        for line in process.stdout:
            log.write(line)
            log.flush()
            try:
                record = json.loads(line)
            except ValueError:
                print(line, end='')
                continue
            if record.get('reason') == 'compiler-message':
                print(record['message'].get('rendered', ''), end='', file=sys.stderr)
            if record.get('reason') == 'compiler-artifact' and record.get('executable'):
                artifacts[record['executable']] = record
        status = process.wait()
    if status:
        return status
    executables = []
    package_dirs = {package['id']: str(Path(package['manifest_path']).parent) for package in metadata['packages']}
    cli = None
    for path, record in sorted(artifacts.items()):
        item = dict(path=path, sha256=digest(path), package_id=record['package_id'],
                    target=record['target']['name'], kinds=record['target']['kind'],
                    manifest_dir=package_dirs[record['package_id']],
                    test=record['profile']['test'], features=record['features'])
        executables.append(item)
        if record['target']['name'] == 'nestweaver' and 'bin' in record['target']['kind'] and not item['test']:
            cli = path
    if cli is None:
        raise RuntimeError('Cargo did not produce the standard CLI executable')
    expected_tests = []
    for package in metadata['packages']:
        if package['id'] not in members:
            continue
        enabled = {feature.split('/', 1)[1] for feature in features if feature.startswith(package['name'] + '/')}
        for target in package['targets']:
            if not target['test'] or not set(target['kind']).intersection({'lib', 'bin', 'test', 'rlib', 'cdylib'}):
                continue
            if not set(target.get('required-features', [])).issubset(enabled):
                continue
            expected_tests.append(dict(package_id=package['id'], target=target['name']))
    actual_tests = {(item['package_id'], item['target']) for item in executables if item['test']}
    missing = [item for item in expected_tests if (item['package_id'], item['target']) not in actual_tests]
    if missing:
        raise RuntimeError(f'Compilation omitted required test executables: {missing}')
    config = tomllib.loads((ROOT / '.cargo/config.toml').read_text())
    runtime_env = {}
    for key, setting in config.get('env', {}).items():
        if isinstance(setting, str):
            runtime_env[key] = environment.get(key, setting)
        else:
            value = setting['value']
            if setting.get('relative'):
                value = str(ROOT / value)
            runtime_env[key] = value if setting.get('force') else environment.get(key, value)
    sysroot = subprocess.check_output(['rustc', '--print', 'sysroot'], text=True).strip()
    host = next(line.split(': ', 1)[1] for line in subprocess.check_output(['rustc', '-vV'], text=True).splitlines() if line.startswith('host: '))
    deps = str(Path(cli).parent / 'deps')
    library_key = 'DYLD_FALLBACK_LIBRARY_PATH' if platform.system() == 'Darwin' else 'LD_LIBRARY_PATH'
    runtime_env[library_key] = os.pathsep.join([deps, f'{sysroot}/lib/rustlib/{host}/lib', environment.get(library_key, '')])
    (args.output / 'artifacts.json').write_text(json.dumps(dict(
        commit=commit, cli=cli, executables=executables, expected_tests=expected_tests, runtime_env=runtime_env), indent=2) + '\n')
    print(f"Retained {len(executables)} executables; CLI: {cli}")
    return 0


if __name__ == '__main__':
    raise SystemExit(main())
