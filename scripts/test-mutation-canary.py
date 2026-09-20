#!/usr/bin/env python3
"""Read-only, offline prerequisite and controlled-result contracts."""
import copy
import subprocess
import importlib.util
from pathlib import Path
import unittest


def load(name):
    path = Path(__file__).with_name(name + '.py')
    assert path.exists(), f'missing {name}'
    spec = importlib.util.spec_from_file_location(name, path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


class Canary(unittest.TestCase):
    def test_prerequisite_binds_all_runtime_jobs_and_current_attempt(self):
        helper = load('verify-mutation-prerequisite')
        sha = 'a' * 40
        run = dict(id=10, run_attempt=1, event='push', head_branch='main', head_sha=sha,
                   path='.github/workflows/ci.yml', status='completed')
        jobs = [dict(id=n + 1, name=name, run_attempt=1, head_sha=sha, status='completed', conclusion='success')
                for n, name in enumerate(helper.JOB_NAMES)]
        def fetch(repo, path):
            if path == 'git/ref/heads/main':
                return [{'object': {'sha': sha}}]
            key = 'jobs' if '/jobs?' in path else 'workflow_runs'
            rows = jobs if key == 'jobs' else [run]
            return [{key: copy.deepcopy(rows), 'total_count': len(rows)}]
        result = helper.observe('owner/repo', sha, 10, fetch)
        self.assertEqual(result['run_id'], 10)
        for mutate in [lambda: jobs[0].update(conclusion='failure'), lambda: jobs[-1].update(conclusion='skipped'),
                       lambda: jobs[-1].update(run_attempt=2), lambda: run.update(head_branch='candidate'),
                       lambda: run.update(run_attempt=2)]:
            original_run, original_jobs = copy.deepcopy(run), copy.deepcopy(jobs)
            mutate()
            with self.assertRaises((ValueError, KeyError)):
                helper.observe('owner/repo', sha, 10, fetch)
            run.clear(); run.update(original_run)
            jobs[:] = original_jobs

    def test_package_selection_is_anchored_and_unique(self):
        helper = load('select-mutation-packages')
        self.assertEqual(helper.select(['src/main.rs', 'src/setup.rs', 'build.rs',
            'crates/nestweaver-canary-sibling/src/lib.rs', 'srcgen/code.rs', 'build.rs.old', 'docs/guide.md']),
            ['nestweaver', 'nestweaver-canary-sibling'])
        self.assertEqual(helper.select(['README.md', 'srcgen/code.rs', 'build.rs.old']), [])

    def test_github_runner_guard_predicates_without_setting_environment(self):
        helper = load('mutation-canary')
        self.assertTrue(hasattr(helper, 'github_runner_context'), 'missing factored GitHub runner predicate')
        complete = dict(GITHUB_ACTIONS='true', RUNNER_TEMP='/runner/temp', RUNNER_OS='Linux', GITHUB_RUN_ID='123')
        cases = [({}, False), ({'CI': '1'}, False), ({'CI': 'true'}, False),
                 ({'GITHUB_ACTIONS': 'true'}, False), (complete, True),
                 (dict(complete, GITHUB_ACTIONS='1'), False)]
        for key in ('RUNNER_TEMP', 'RUNNER_OS', 'GITHUB_RUN_ID'):
            incomplete = dict(complete)
            incomplete.pop(key)
            cases.append((incomplete, False))
        predicates = []
        for name in ('ci-direct-cargo.sh', 'run-mutation-scope.sh'):
            source = Path(__file__).with_name(name).read_text()
            self.assertIn('# BEGIN RUNNER PREDICATE', source)
            predicate = source.split('# BEGIN RUNNER PREDICATE\n', 1)[1].split('# END RUNNER PREDICATE', 1)[0]
            predicates.append(predicate)
            # Evaluate only a function receiving inert strings as positional
            # arguments. Do not execute the guard caller, set CI/runner env,
            # resolve Cargo or enter any guarded command path.
            for data, expected in cases:
                with self.subTest(script=name, data=data):
                    result = subprocess.run(['bash', '-c', predicate + 'github_runner_context "$@"', 'predicate',
                        *[data.get(key, '') for key in ('GITHUB_ACTIONS', 'RUNNER_TEMP', 'RUNNER_OS', 'GITHUB_RUN_ID')]],
                        capture_output=True, timeout=5)
                    self.assertEqual(result.returncode == 0, expected)
                    self.assertEqual(helper.github_runner_context(data), expected)
            call = source.index('if ! github_runner_context "${GITHUB_ACTIONS:-}"')
            self.assertLess(call, source.index('NW_REAL_CARGO') if name == 'ci-direct-cargo.sh' else source.index("packages='"))
            self.assertNotIn('${CI:-}', source)
        self.assertEqual(*predicates)
        source = Path(__file__).with_name('mutation-canary.py').read_text().split('def run_cases(evidence):', 1)[1]
        self.assertLess(source.index('if not github_runner_context(os.environ):'), source.index('fixture ='))

    def test_exact_case_counts(self):
        helper = load('mutation-canary')
        base = dict(planned=2, accounted=2, caught=2, survived=0, timed_out=0, unviable=0,
                    outstanding=0, status=0, exit_status=0, outer_exhausted=False, complete=True,
                    reason='complete mutant accounting')
        helper.check_counts('complete', base)
        for key in ('planned', 'accounted', 'caught', 'survived', 'timed_out', 'unviable', 'outstanding', 'status', 'exit_status'):
            altered = dict(base); altered[key] += 1
            with self.subTest(key=key), self.assertRaises(ValueError):
                helper.check_counts('complete', altered)
        for count in (0, 1):
            negative = dict(base, planned=1, accounted=count, caught=0, timed_out=count, outstanding=1-count,
                            status=124, exit_status=1, outer_exhausted=True, complete=False,
                            reason='outer budget exhausted')
            helper.check_counts('exhausted', negative)
            negative['outer_exhausted'] = False
            with self.assertRaises(ValueError):
                helper.check_counts('exhausted', negative)


if __name__ == '__main__':
    unittest.main(verbosity=2)
