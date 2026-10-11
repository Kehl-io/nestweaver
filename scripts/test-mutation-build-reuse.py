#!/usr/bin/env python3
"""Offline build-graph and doctest-preservation contracts; no forged CI."""
import importlib.util
import os
from pathlib import Path
import subprocess
import sys
import unittest


def load(name):
    spec = importlib.util.spec_from_file_location(name, Path(__file__).with_name(name + '.py'))
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


class BuildReuse(unittest.TestCase):
    def test_internal_entry_points_refuse_missing_runner_context(self):
        environment = dict(os.environ)
        for key in ('GITHUB_ACTIONS', 'RUNNER_TEMP', 'RUNNER_OS', 'GITHUB_RUN_ID'):
            environment.pop(key, None)
        for name in ('prewarm-mutation-build', 'ci-nextest-with-docs'):
            result = subprocess.run([sys.executable, str(Path(__file__).with_name(name + '.py'))],
                                    env=environment, capture_output=True, text=True, timeout=5)
            self.assertNotEqual(result.returncode, 0)
            self.assertIn('require GitHub Actions runner context', result.stderr)

    def test_prewarm_uses_one_complete_package_union(self):
        helper = load('prewarm-mutation-build')
        packages = ['nestweaver-engine', 'nestweaver-store']
        command = helper.build_command(packages)
        self.assertEqual(command, ['cargo', 'nextest', 'run', '--locked', '--no-run', '--verbose',
                                   '--package', packages[0], '--package', packages[1]])
        root = helper.build_command(['nestweaver', *packages])
        self.assertEqual(root[-2:], ['--features', 'nestweaver/ci-direct-tests'])
        for malformed in ([], ['dup', 'dup'], ['--workspace'], ['package; echo bad']):
            with self.assertRaises(ValueError):
                helper.build_command(malformed)

    def test_docs_keep_package_feature_target_and_profile_selection(self):
        helper = load('ci-nextest-with-docs')
        self.assertEqual(helper.doctest_command('/cargo', ['nextest', 'run', '--locked',
            '--package=engine', '-p', 'store', '--features', 'embed', '--all-features',
            '--cargo-profile=test', '--target', 'example', '--fail-fast']),
            ['/cargo', 'test', '--doc', '--locked', '--package', 'engine', '-p', 'store',
             '--features', 'embed', '--all-features', '--profile', 'test', '--target', 'example'])
        self.assertIsNone(helper.doctest_command('/cargo', ['nextest', 'run', '--no-run']))
        for arguments in (['test'], ['nextest', 'run', '--package'],
                          ['nextest', 'run', '--unexpected-filter']):
            with self.assertRaises(ValueError):
                helper.doctest_command('/cargo', arguments)

    def test_fail_fast_still_detects_doctest_failures(self):
        helper = load('ci-nextest-with-docs')
        for results, expected, calls in [([100], 100, 1), ([0, 101], 100, 2),
                                          ([0, 0], 0, 2), ([-2], 130, 1)]:
            commands = []
            statuses = iter(results)
            def run(command):
                commands.append(command)
                return next(statuses)
            self.assertEqual(helper.run_commands('/cargo', ['nextest', 'run', '-p', 'engine',
                                  '--fail-fast'], run), expected)
            self.assertEqual(len(commands), calls)
            if calls == 2:
                self.assertEqual(commands[1], ['/cargo', 'test', '--doc', '-p', 'engine'])


if __name__ == '__main__':
    unittest.main(verbosity=2)
