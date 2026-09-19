#!/usr/bin/env python3
"""Offline classifier regressions; generated records below are unit-test data only."""
import copy
import importlib.util
import json
from pathlib import Path
import tempfile
import unittest

SCRIPT = Path(__file__).with_name('report-mutation-results.py')


class Results(unittest.TestCase):
    def classify(self, planned=2, verdicts=('CaughtMutant',), status=0, edit=None, **kwargs):
        self.assertTrue(SCRIPT.exists(), 'shared production classifier is missing')
        spec = importlib.util.spec_from_file_location('report', SCRIPT)
        module = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(module)
        plan = [dict(name=f'src/lib.rs:{n}:1: replace ready with false', package='nestweaver',
                     file='src/lib.rs', replacement='false', genre='FnValue', span={'start': {'line': n, 'column': 1}})
                for n in range(planned)]
        keys = {'CaughtMutant': 'caught', 'MissedMutant': 'missed', 'Timeout': 'timeout', 'Unviable': 'unviable'}
        report = dict(caught=0, missed=0, timeout=0, unviable=0, total_mutants=len(verdicts), outcomes=[])
        for n, verdict in enumerate(verdicts):
            report[keys[verdict]] += 1
            phase = dict(phase='Build' if verdict == 'Unviable' else 'Test', duration=0.1,
                         process_status='Timeout' if verdict == 'Timeout' else 'Success' if verdict == 'MissedMutant' else {'Failure': 101},
                         argv=['cargo', 'test', '--package=nestweaver'])
            report['outcomes'].append(dict(scenario={'Mutant': copy.deepcopy(plan[n])}, summary=verdict, phase_results=[phase]))
        state = dict(status=status, truncated=status in (124, 137), no_packages=False)
        data = {'plan': plan, 'report': report, 'state': state}
        if edit:
            edit(data)
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            for name, value in [('mutants.json', data['plan']), ('outcomes.json', data['report'])]:
                if value is not None:
                    (root / name).write_text(json.dumps(value))
            return module.classify(root, data['state'], **kwargs)

    def test_partial_accounting_fails(self):
        result = self.classify()
        self.assertEqual(result['exit_status'], 1)
        self.assertEqual(result['outstanding'], 1)
        self.assertIn('incomplete', result['reason'])

    def test_complete_timeout_is_not_outer_exhaustion(self):
        result = self.classify(verdicts=('CaughtMutant', 'Timeout'), status=3)
        self.assertEqual(result['exit_status'], 0)
        self.assertIn('complete accounting with mutation timeouts', result['reason'])
        self.assertFalse(result['outer_exhausted'])

    def test_outer_exhaustion_fails_even_with_recorded_timeout(self):
        for verdicts in ((), ('Timeout',)):
            with self.subTest(verdicts=verdicts):
                result = self.classify(planned=1, verdicts=verdicts, status=124)
                self.assertEqual(result['exit_status'], 1)
                self.assertEqual(result['accounted'], len(verdicts))
                self.assertEqual(result['outstanding'], 1 - len(verdicts))
                self.assertTrue(result['outer_exhausted'])
                self.assertIn('outer budget', result['reason'])

    def test_complete_caught_and_survivors_are_advisory(self):
        for verdicts, status in [(('CaughtMutant', 'CaughtMutant'), 0), (('CaughtMutant', 'MissedMutant'), 2)]:
            self.assertEqual(self.classify(verdicts=verdicts, status=status)['exit_status'], 0)

    def test_unverifiable_reports_fail(self):
        edits = [lambda d: d.update(plan=None), lambda d: d.update(plan={}),
                 lambda d: d.update(report=None), lambda d: d['report'].update(caught=-1),
                 lambda d: d['report'].update(caught='1'), lambda d: d['report'].update(caught=True),
                 lambda d: d['report'].update(caught=3), lambda d: d['report'].update(total_mutants=1),
                 lambda d: d['plan'].__setitem__(1, copy.deepcopy(d['plan'][0])),
                 lambda d: d['report']['outcomes'].append(copy.deepcopy(d['report']['outcomes'][0])),
                 lambda d: d['report']['outcomes'][0]['scenario']['Mutant'].update(name='unrelated'),
                 lambda d: d['report']['outcomes'][0].update(phase_results=[]),
                 lambda d: d['report']['outcomes'][0]['phase_results'][0].update(process_status='Success')]
        for edit in edits:
            with self.subTest(edit=edit):
                self.assertEqual(self.classify(verdicts=('CaughtMutant', 'CaughtMutant'), edit=edit)['exit_status'], 1)

    def test_complete_exit_and_verdicts_must_agree(self):
        self.assertEqual(self.classify(verdicts=('CaughtMutant', 'Timeout'), status=0)['exit_status'], 1)
        self.assertEqual(self.classify(verdicts=('CaughtMutant', 'CaughtMutant'), status=3)['exit_status'], 1)

    def test_zero_cannot_hide_nonzero_report(self):
        self.assertEqual(self.classify(planned=0, verdicts=(), discovery=[], console='No mutants to filter',
                         edit=lambda d: d['report'].update(caught=1))['exit_status'], 1)

    def test_zero_requires_authentic_discovery_and_recognized_log(self):
        self.assertEqual(self.classify(planned=0, verdicts=(), discovery=[], console='No mutants found under the active filters')['exit_status'], 1)
        result = self.classify(planned=0, verdicts=(), discovery=[], console='No mutants to filter')
        self.assertEqual(result['exit_status'], 0)
        self.assertIn('legitimate-zero', result['reason'])
        self.assertEqual(self.classify(planned=0, verdicts=())['exit_status'], 1)


if __name__ == '__main__':
    unittest.main(verbosity=2)
