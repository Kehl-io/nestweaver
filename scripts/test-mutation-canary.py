#!/usr/bin/env python3
"""Read-only, offline prerequisite and controlled-result contracts."""
import copy
import subprocess
import importlib.util
from pathlib import Path
import unittest
import tempfile
from unittest.mock import patch


def load(name):
    path = Path(__file__).with_name(name + '.py')
    assert path.exists(), f'missing {name}'
    spec = importlib.util.spec_from_file_location(name, path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


class Canary(unittest.TestCase):
    def test_versions_use_each_tools_cli_protocol(self):
        helper = load('mutation-canary')
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            for name, args, output in (
                ('cargo', '--version', 'cargo 1.99.0'),
                ('rustc', '--version', 'rustc 1.99.0'),
                ('cargo-mutants', 'mutants --version', 'cargo-mutants 27.1.0'),
                ('cargo-nextest', '--version', 'cargo-nextest 0.9.140 (fixture)'),
            ):
                executable = root / name
                executable.write_text('#!/bin/sh\n[ "$*" = "' + args + '" ] || exit 2\nprintf "%s\\n" "' + output + '"\n')
                executable.chmod(0o755)
            with patch.dict('os.environ', {'PATH': str(root)}):
                versions = helper.tool_versions(str(root / 'cargo'))
            self.assertEqual(versions['cargo-mutants'], 'cargo-mutants 27.1.0')
            self.assertEqual(versions['cargo-nextest'], 'cargo-nextest 0.9.140 (fixture)')

    def test_trace_rejects_package_graph_and_runner_drift(self):
        helper = load('mutation-canary')
        packages = ['--package=nestweaver@0.0.0', '--package=nestweaver-canary-sibling@0.0.0']
        phases = []
        records = []
        for phase in ('Build', 'Test'):
            argv = ['nextest', 'run', *packages] + (['--no-run'] if phase == 'Build' else ['--fail-fast'])
            phases.append(dict(phase=phase, argv=['/shim/cargo', *argv]))
            records.append(dict(original_argv=argv, effective_argv=[*argv, '--features', 'ci-direct-tests'],
                                injected_feature=True))
        result = dict(verdicts=[dict(phase_results=copy.deepcopy(phases)) for _ in range(2)])
        helper.check_trace(records, result)
        for replacement in (['test', *packages], ['nextest', 'run', packages[0]]):
            altered = copy.deepcopy(result)
            altered['verdicts'][0]['phase_results'][0]['argv'] = ['/shim/cargo', *replacement]
            with self.assertRaises(ValueError):
                helper.check_trace(records, altered)
        altered = copy.deepcopy(records)
        altered[0]['injected_feature'] = False
        with self.assertRaises(ValueError):
            helper.check_trace(altered, result)

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

    def test_selected_branch_prerequisite_requires_actual_runtime_jobs(self):
        helper = load('verify-mutation-prerequisite')
        sha = 'b' * 40
        run = dict(id=20, run_attempt=1, event='pull_request', head_branch='candidate',
                   head_sha=sha, path='.github/workflows/ci.yml', status='completed', conclusion='success')
        jobs = [dict(id=n + 1, name=name, run_attempt=1, head_sha=sha,
                     status='completed', conclusion='success') for n, name in enumerate(helper.JOB_NAMES)]
        calls = []
        def fetch(repo, path):
            calls.append(path)
            if path == 'actions/runs/20':
                return [copy.deepcopy(run)]
            self.assertEqual(path, 'actions/runs/20/attempts/1/jobs?per_page=100')
            return [dict(jobs=copy.deepcopy(jobs[:2]), total_count=len(jobs)),
                    dict(jobs=copy.deepcopy(jobs[2:]), total_count=len(jobs))]
        result = helper.observe_selected('owner/repo', sha, 20, fetch)
        self.assertEqual([job['id'] for job in result['jobs']], [1, 2, 3, 4])
        self.assertEqual(calls.count('actions/runs/20'), 2)
        run['path'] = '.github/workflows/ci.yml@refs/heads/candidate'
        self.assertEqual(helper.observe_selected('owner/repo', sha, 20, fetch)['run_id'], 20)
        run['path'] = '.github/workflows/ci.yml'
        for mutate in (lambda: jobs.pop(),
                       lambda: jobs[-1].update(conclusion='skipped'),
                       lambda: jobs[-1].update(head_sha='c' * 40),
                       lambda: jobs[-1].update(run_attempt=2),
                       lambda: jobs.append(dict(jobs[-1], id=99))):
            original = copy.deepcopy(jobs)
            mutate()
            with self.assertRaises(ValueError):
                helper.observe_selected('owner/repo', sha, 20, fetch)
            jobs[:] = original
        for mutate in (lambda: run.update(path='.github/workflows/coverage.yml'),
                       lambda: run.update(head_sha='c' * 40),
                       lambda: run.update(status='in_progress', conclusion=None),
                       lambda: run.update(conclusion='failure')):
            original = copy.deepcopy(run)
            mutate()
            with self.assertRaises(ValueError):
                helper.observe_selected('owner/repo', sha, 20, fetch)
            run.clear(); run.update(original)

    def test_selected_prerequisite_rejects_attempt_drift_and_truncated_jobs(self):
        helper = load('verify-mutation-prerequisite')
        sha = 'b' * 40
        for drift in ('rerun', 'pending', 'truncated'):
            observations = 0
            def fetch(repo, path):
                nonlocal observations
                if path == 'actions/runs/20':
                    observations += 1
                    run = dict(id=20, run_attempt=1, head_sha=sha, path='.github/workflows/ci.yml',
                               status='completed', conclusion='success')
                    if observations == 2 and drift == 'rerun':
                        run['run_attempt'] = 2
                    if observations == 2 and drift == 'pending':
                        run.update(status='in_progress', conclusion=None)
                    return [run]
                jobs = [dict(id=n + 1, name=name, run_attempt=1, head_sha=sha,
                             status='completed', conclusion='success')
                        for n, name in enumerate(helper.JOB_NAMES)]
                return [dict(jobs=jobs, total_count=len(jobs) + (drift == 'truncated'))]
            with self.subTest(drift=drift), self.assertRaises(ValueError):
                helper.observe_selected('owner/repo', sha, 20, fetch)

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
