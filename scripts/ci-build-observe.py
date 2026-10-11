#!/usr/bin/env python3
"""Record Cargo compile evidence without changing build features or exit status."""
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import time


def snapshot(target):
    # Fingerprint names/hashes and build-script output expose native build
    # shape changes without copying multi-GB object trees into an artifact.
    result = {}
    for package in ('lbug', 'cc', 'cmake', 'cxx-build', 'openssl-sys'):
        for path in sorted(target.glob(f'**/.fingerprint/{package}-*/*.json')):
            try:
                result[str(path)] = json.loads(path.read_text())
            except (OSError, ValueError) as error:
                result[str(path)] = {'read_error': type(error).__name__}
        for path in sorted(target.glob(f'**/build/{package}-*/output')):
            try:
                result[str(path)] = path.read_text(errors='replace')
            except OSError as error:
                result[str(path)] = {'read_error': type(error).__name__}
    return result


def main():
    if len(sys.argv) < 5:
        raise SystemExit('usage: ci-build-observe.py LABEL -- cargo build|test ...')
    label, separator, *command = sys.argv[1:]
    if separator != '--' or not re.fullmatch(r'[a-z0-9-]+', label):
        raise SystemExit('usage: ci-build-observe.py LABEL -- cargo build|test ...')
    if len(command) < 2 or command[:1] != ['cargo'] or command[1] not in ('build', 'test'):
        raise SystemExit('only cargo build/test commands are supported')
    # Insert before the test-harness separator, never among test arguments.
    index = command.index('--') if '--' in command else len(command)
    command.insert(index, '--timings')
    evidence = Path(os.environ.get('RUNNER_TEMP', 'target')) / 'metal-build-evidence'
    evidence.mkdir(parents=True, exist_ok=True)
    target = Path(os.environ.get('CARGO_TARGET_DIR', 'target'))
    record = {'command': command, 'before': snapshot(target)}
    for name, probe in (('rustc', ['rustc', '-vV']), ('clang', ['clang', '--version']),
                        ('cmake', ['cmake', '--version']), ('disk', ['df', '-h', '.']),
                        ('target_kib_before', ['du', '-sk', str(target)])):
        try:
            result = subprocess.run(probe, capture_output=True, text=True, check=False)
            record[name] = result.stdout + result.stderr
        except OSError as error:
            record[name] = {'probe_error': type(error).__name__}
    source = os.environ.get('LBUG_SOURCE_DIR')
    if source:
        result = subprocess.run(['git', '-C', source, 'rev-parse', 'HEAD'],
                                capture_output=True, text=True, check=False)
        record['lbug_source'] = {'path': source, 'commit': result.stdout.strip()}
    # Only named build inputs are recorded; never dump the runner environment.
    record['inputs'] = {name: os.environ.get(name) for name in (
        'CFLAGS', 'CXXFLAGS', 'CC', 'CXX', 'MACOSX_DEPLOYMENT_TARGET',
        'CARGO_PROFILE_RELEASE_LTO', 'CARGO_PROFILE_RELEASE_DEBUG', 'RUSTFLAGS',
        'CARGO_TARGET_AARCH64_APPLE_DARWIN_RUSTFLAGS', 'LBUG_BUILD_FROM_SOURCE')}
    env = os.environ.copy()
    env['CARGO_LOG'] = 'cargo::core::compiler::fingerprint=info'
    start = time.monotonic()
    with (evidence / f'{label}.log').open('w') as log:
        process = subprocess.Popen(command, stdout=subprocess.PIPE,
                                   stderr=subprocess.STDOUT, text=True, env=env)
        for line in process.stdout:
            log.write(line)
            print(line, end='', flush=True)
        code = process.wait()
    record.update(seconds=time.monotonic() - start, exit_code=code, after=snapshot(target))
    result = subprocess.run(['du', '-sk', str(target)], capture_output=True, text=True, check=False)
    record['target_kib_after'] = result.stdout + result.stderr
    (evidence / f'{label}.json').write_text(json.dumps(record, indent=2) + '\n')
    summary = os.environ.get('GITHUB_STEP_SUMMARY')
    if summary:
        with open(summary, 'a') as out:
            out.write(f'\nMetal `{label}`: {record["seconds"]:.1f}s, exit {code}.\n')
    return code if code >= 0 else 128 - code


if __name__ == '__main__':
    sys.exit(main())
