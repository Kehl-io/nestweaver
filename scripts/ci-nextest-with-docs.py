#!/usr/bin/env python3
"""Keep doctest mutation detection after a successful nextest test phase."""
import os
import json
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


def metadata_command(cargo, docs):
    command = [cargo, 'metadata', '--format-version', '1', '--no-deps']
    for index, arg in enumerate(docs):
        if arg in ('--locked', '--offline', '--frozen'):
            command.append(arg)
        elif arg == '--manifest-path':
            command.extend([arg, docs[index + 1]])
    return command


def has_doctest_targets(metadata, docs):
    selected = []
    for index, arg in enumerate(docs):
        if arg in ('--package', '-p'):
            selected.append(docs[index + 1].split('@')[0])
    packages = metadata['packages']
    if selected:
        packages = [p for p in packages if p['name'] in selected]
        if {p['name'] for p in packages} != set(selected):
            raise ValueError('doctest package selection is missing from metadata')
    else:
        members = metadata['workspace_members'] if '--workspace' in docs else metadata['workspace_default_members']
        packages = [p for p in packages if p['id'] in members]
    if not packages:
        raise ValueError('empty doctest package selection')
    return any(target['doctest'] and set(target['kind']) & {'lib', 'rlib', 'dylib', 'cdylib', 'staticlib', 'proc-macro'}
               for package in packages for target in package['targets'])


def read_metadata(command):
    return json.loads(subprocess.check_output(command, text=True))


def run_commands(cargo, arguments, run=subprocess.call, metadata=read_metadata):
    docs = doctest_command(cargo, arguments)
    status = run([cargo, *arguments])
    if status == 0 and docs is not None:
        if not has_doctest_targets(metadata(metadata_command(cargo, docs)), docs):
            print('selected packages have no doctest targets', flush=True)
            return 0
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
