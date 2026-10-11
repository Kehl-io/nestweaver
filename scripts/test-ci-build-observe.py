#!/usr/bin/env python3
"""Exercise observation against a fake Cargo process, including failure."""
import json
import importlib.util
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

SCRIPT = Path(__file__).with_name('ci-build-observe.py')


class ObservationTests(unittest.TestCase):
    def test_incomplete_fingerprint_is_evidence_not_a_build_failure(self):
        spec = importlib.util.spec_from_file_location('observe', SCRIPT)
        observe = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(observe)
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / 'release/.fingerprint/lbug-example/lib-lbug.json'
            path.parent.mkdir(parents=True)
            path.write_text('{unfinished')
            self.assertEqual(observe.snapshot(Path(tmp))[str(path)],
                             {'read_error': 'JSONDecodeError'})

    def test_success_and_failure_keep_arguments_and_evidence(self):
        for code in (0, 101):
            with self.subTest(code=code), tempfile.TemporaryDirectory() as tmp:
                root = Path(tmp)
                cargo = root / 'cargo'
                cargo.write_text('#!/bin/sh\nprintf "observed-output\\n"\nexit ' + str(code) + '\n')
                cargo.chmod(0o755)
                env = os.environ.copy()
                env.update(PATH=tmp + os.pathsep + env['PATH'], RUNNER_TEMP=tmp,
                           CARGO_TARGET_DIR=str(root / 'target'),
                           GITHUB_STEP_SUMMARY=str(root / 'summary'))
                command = ['cargo', 'test', '--locked', '--release', '--features',
                           'metal', '--', 'named_test', '--exact']
                result = subprocess.run([sys.executable, str(SCRIPT), 'probe', '--', *command],
                                        env=env, capture_output=True, text=True, timeout=20)
                self.assertEqual(result.returncode, code, result.stderr)
                evidence = root / 'metal-build-evidence'
                record = json.loads((evidence / 'probe.json').read_text())
                self.assertEqual(record['command'], command[:6] + ['--timings'] + command[6:])
                self.assertEqual(record['exit_code'], code)
                self.assertIn('observed-output', result.stdout)
                self.assertIn('observed-output', (evidence / 'probe.log').read_text())
                self.assertIn('exit ' + str(code), (root / 'summary').read_text())


if __name__ == '__main__':
    unittest.main()
