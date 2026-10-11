#!/usr/bin/env python3
"""Keep doctest mutation detection after a successful nextest test phase."""
import os
import subprocess
import sys


def doctest_command(cargo, arguments):
    if arguments[:2] != ['nextest', 'run']:
        raise ValueError('expected nextest run')
    if '--no-run' in arguments:
        return None
    command = [cargo, 'test', '--doc']
    flags = {'--locked', '--offline', '--frozen', '--verbose', '--release',
             '--all-features', '--no-default-features', '--workspace'}
    values = {'--package', '-p', '--features', '--target', '--target-dir', '--manifest-path'}
    index = 2
    while index < len(arguments):
        arg = arguments[index]
        name, equal, value = arg.partition('=')
        if arg == '--fail-fast':
            index += 1
            continue
        if name == '--cargo-profile':
            name = '--profile'
        elif arg in flags:
            command.append(arg)
            index += 1
            continue
        elif name not in values:
            raise ValueError(f'unsupported mutation nextest argument: {arg}')
        if equal:
            command.extend([name, value])
        else:
            index += 1
            if index >= len(arguments):
                raise ValueError(f'missing value for {arg}')
            command.extend([name, arguments[index]])
        index += 1
    return command


def run_commands(cargo, arguments, run=subprocess.call):
    docs = doctest_command(cargo, arguments)
    status = run([cargo, *arguments])
    if status == 0 and docs is not None:
        print('nextest passed; running the same packages\' doctests', flush=True)
        status = run(docs)
        if status > 0:
            return 100
    return status if status >= 0 else 128 - status


def main():
    # This adapter is internal to the guarded GitHub Cargo shim. Never assign
    # runner markers to exercise it locally; test the pure argument projection.
    if not (os.environ.get('GITHUB_ACTIONS') == 'true' and all(
            os.environ.get(key) for key in ('RUNNER_TEMP', 'RUNNER_OS', 'GITHUB_RUN_ID'))):
        raise SystemExit('mutation helpers require GitHub Actions runner context')
    cargo, *arguments = sys.argv[1:]
    return run_commands(cargo, arguments)


if __name__ == '__main__':
    sys.exit(main())
