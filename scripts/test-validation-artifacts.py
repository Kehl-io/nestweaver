#!/usr/bin/env python3
"""Run previously compiled Rust tests, verifying hashes and commit; never invoke Cargo."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import subprocess

ROOT = Path(__file__).resolve().parents[1]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--manifest', required=True, type=Path)
    parser.add_argument('--threads', type=int, default=2)
    parser.add_argument('--exclude-target', action='append', default=[],
                        choices=['cli_test', 'error_remedy_test', 'publication_creation_test', 'ready_regression_test', 'server_test'], help='Legacy CI-only bypass fixtures; explicitly record exclusion')
    args = parser.parse_args()
    manifest = json.loads(args.manifest.read_text())
    commit = subprocess.check_output(['git', 'rev-parse', 'HEAD'], cwd=ROOT, text=True).strip()
    if manifest['commit'] != commit or subprocess.check_output(['git', 'status', '--porcelain', '--untracked-files=all'], cwd=ROOT):
        parser.error('source changed after the frozen build')
    # Verify every artifact before starting any test.
    for item in manifest['executables']:
        value = hashlib.sha256()
        with open(item['path'], 'rb') as stream:
            for chunk in iter(lambda: stream.read(1024 * 1024), b''):
                value.update(chunk)
        if value.hexdigest() != item['sha256']:
            parser.error(f"artifact changed: {item['path']}")
    environment = os.environ.copy()
    environment.update(manifest['runtime_env'])
    results = []
    for item in manifest['executables']:
        if not item['test']:
            continue
        if item['target'] in args.exclude_target:
            results.append(dict(target=item['target'], path=item['path'], skipped=True,
                reason='Legacy direct-mode helper requires CI-only artifact and runner; tracked low-priority fixture debt. Real daemon acceptance runs separately.'))
            continue
        environment['CARGO_MANIFEST_DIR'] = item['manifest_dir']
        log = args.manifest.parent / f"test-{Path(item['path']).name}.log"
        with log.open('w') as stream:
            status = subprocess.run([item['path'], '--test-threads', str(args.threads)],
                                    cwd=item['manifest_dir'], env=environment, stdout=stream, stderr=subprocess.STDOUT).returncode
        print(f"{item['target']}: exit {status} ({log})", flush=True)
        results.append(dict(target=item['target'], path=item['path'], status=status, log=str(log)))
    (args.manifest.parent / 'test-results.json').write_text(json.dumps(results, indent=2) + '\n')
    return int(any(item.get('status', 0) for item in results))


if __name__ == '__main__':
    raise SystemExit(main())
