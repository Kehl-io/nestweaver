#!/usr/bin/env python3
"""Warm one unified test-package graph, matching run-mutation-scope.sh."""
import os
from pathlib import Path
import re
import subprocess
import sys


def build_command(packages):
    if not packages or len(set(packages)) != len(packages):
        raise ValueError('expected a nonempty unique package list')
    if any(not re.fullmatch(r'[a-zA-Z0-9_][a-zA-Z0-9_-]*', package) for package in packages):
        raise ValueError('invalid package name')
    command = ['cargo', 'nextest', 'run', '--locked', '--no-run', '--verbose']
    for package in packages:
        command.extend(['--package', package])
    if 'nestweaver' in packages:
        command.extend(['--features', 'nestweaver/ci-direct-tests'])
    return command


def main():
    # Never forge these values to exercise a CI-only internal build locally.
    if not (os.environ.get('GITHUB_ACTIONS') == 'true' and all(
            os.environ.get(key) for key in ('RUNNER_TEMP', 'RUNNER_OS', 'GITHUB_RUN_ID'))):
        raise SystemExit('mutation helpers require GitHub Actions runner context')
    if len(sys.argv) != 2:
        raise SystemExit('usage: prewarm-mutation-build.py PACKAGES_FILE')
    packages = Path(sys.argv[1]).read_text().splitlines()
    command = build_command(packages)
    print('pre-warming one unified test graph: ' + ', '.join(packages), flush=True)
    # Reuse workspace compilation between mutants inside this job. No new
    # persistent workspace cache, compiler profile or native flags are added.
    env = dict(os.environ, CARGO_INCREMENTAL='1')
    return subprocess.call(command, env=env)


if __name__ == '__main__':
    sys.exit(main())
