#!/usr/bin/env python3
"""Offline release-gate regressions. No daemon, registry, or GitHub access."""
import base64
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[1]


def load(name):
    spec = importlib.util.spec_from_file_location(name, ROOT / 'scripts' / f'{name}.py')
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


ci = load('verify-release-required-ci')
npm = load('verify-npm-immutable')
SHA = 'a' * 40
INTEGRITY = 'sha512-' + base64.b64encode(b'a' * 64).decode()
WRONG = 'sha512-' + base64.b64encode(b'b' * 64).decode()
META = {'name': 'nestweaver', 'version': '1.0.0', 'dist.integrity': INTEGRITY}


def run(**changes):
    return dict(id=10, head_sha=SHA, event='push', path='.github/workflows/ci.yml',
                status='in_progress', run_attempt=1, html_url='fixture:run') | changes


def job(**changes):
    return dict(id=20, head_sha=SHA, name='Required CI', run_attempt=1,
                status='completed', conclusion='success', html_url='fixture:job') | changes


def pages(key, items, size=100):
    return [{key: items[i:i + size], 'total_count': len(items)}
            for i in range(0, max(1, len(items)), size)]


class RequiredCI(unittest.TestCase):
    def check(self, expected, r=None, jobs=None):
        self.assertEqual(ci.classify(r or run(), pages('jobs', jobs or []), SHA)[0], expected)

    def test_no_runs_and_wrong_identity(self):
        variants = [[], [run(head_sha='b' * 40)], [run(head_sha=SHA[:8])],
                    [run(event='pull_request')], [run(event='workflow_dispatch')],
                    [run(path='.github/workflows/other.yml')]]
        for items in variants:
            with self.subTest(items=items):
                self.assertIsNone(ci.select_run(pages('workflow_runs', items), SHA))

    def test_newest_run_all_pages(self):
        items = [run(id=i) for i in range(1, 202)]
        self.assertEqual(ci.select_run(pages('workflow_runs', items), SHA)['id'], 201)
        self.assertEqual(ci.select_run(pages('workflow_runs', [run(path='.github/workflows/ci.yml@refs/heads/main')]), SHA)['id'], 10)

    def test_missing_check(self):
        for status in ci.PENDING:
            self.check('pending', run(status=status))
        self.check('fail', run(status='completed'))

    def test_pending_check(self):
        for status in ci.PENDING:
            self.check('pending', jobs=[job(status=status, conclusion=None)])
        self.check('fail', jobs=[job(status='unknown')])

    def test_terminal_required_conclusions(self):
        for conclusion in ['failure', 'cancelled', 'timed_out', 'action_required', 'stale',
                           'startup_failure', 'neutral', 'skipped', None, 'unknown']:
            with self.subTest(conclusion=conclusion):
                self.check('fail', jobs=[job(conclusion=conclusion)])

    def test_advisory_is_irrelevant(self):
        for status, conclusion in [('queued', None), ('in_progress', None),
                                   ('completed', 'failure'), ('completed', 'cancelled')]:
            self.check('pass', run(status='completed', conclusion='failure'),
                       [job(), job(id=21, name='Coverage', status=status, conclusion=conclusion)])

    def test_attempts(self):
        self.check('pending', run(run_attempt=2), [job()])
        self.check('fail', run(run_attempt=2, status='completed'), [job()])
        self.check('pending', run(run_attempt=2), [job(), job(id=21, run_attempt=2, status='queued', conclusion=None)])
        self.check('fail', run(run_attempt=2), [job(), job(id=21, run_attempt=2, conclusion='failure')])
        self.check('pass', run(run_attempt=2), [job(conclusion='failure'), job(id=21, run_attempt=2)])

    def test_invalid_evidence(self):
        for items in [[job(head_sha='b' * 40)], [job(run_attempt=2)],
                      [job(), job(id=21)], [job(id='20')], [job(), job()]]:
            with self.subTest(items=items), self.assertRaises(ValueError):
                ci.classify(run(), pages('jobs', items), SHA)
        for data in [[], {}, [{'jobs': [], 'total_count': 2}],
                     [{'jobs': [], 'total_count': '0'}]]:
            with self.assertRaises(ValueError):
                ci.classify(run(), data, SHA)

    def test_paginated_jobs(self):
        jobs = [job(id=i, name='advisory') for i in range(1, 150)] + [job(id=200)]
        self.assertEqual(ci.classify(run(), pages('jobs', jobs), SHA)[0], 'pass')

    def test_poll_revalidates_current_run_and_attempt(self):
        for changed in [run(run_attempt=2), run(id=11)]:
            responses = iter([pages('workflow_runs', [run()]), pages('jobs', [job()]),
                              pages('workflow_runs', [changed])])
            self.assertEqual(ci.poll('fixture/repo', SHA, lambda *_: next(responses))[0], 'pending')

    def test_poll_uses_newest_and_all_attempts(self):
        paths = []
        def fetch(_, path):
            paths.append(path)
            return pages('jobs', []) if '/jobs?' in path else pages('workflow_runs', [run(id=9, status='completed'), run()])
        self.assertEqual(ci.poll('fixture/repo', SHA, fetch)[0], 'pending')
        self.assertIn('runs/10/jobs?filter=all&per_page=100', paths[1])

    def test_api_pagination_and_timeout(self):
        with patch.object(ci.subprocess, 'run', return_value=subprocess.CompletedProcess([], 0, '[]')) as call:
            ci.api('fixture/repo', 'path')
        self.assertIn('--paginate', call.call_args.args[0])
        self.assertIn('--slurp', call.call_args.args[0])
        self.assertEqual(call.call_args.kwargs['timeout'], 60)


class Registry(unittest.TestCase):
    def check(self, expected, code=0, data=META):
        self.assertEqual(npm.classify(code, json.dumps(data), 'nestweaver', '1.0.0', INTEGRITY)[0], expected)

    def test_exact(self):
        self.check('exact')

    def test_invalid_is_terminal(self):
        for data in [[], None, META | {'name': 'wrong'}, META | {'version': '2'},
                     META | {'dist.integrity': WRONG}, META | {'dist.integrity': None},
                     META | {'dist.integrity': 'sha512-bad'}, {'name': 'nestweaver', 'version': '1.0.0'}]:
            with self.subTest(data=data):
                self.check('invalid', data=data)
        self.assertEqual(npm.classify(0, '{', 'nestweaver', '1.0.0', INTEGRITY), ('invalid', 'malformed-json'))

    def test_retryable_errors(self):
        for error in ['E404', 'ETARGET', 'E500', 'E503', 'ETIMEDOUT', 'ECONNRESET', 'ENOTFOUND']:
            self.check('retry', 1, {'error': {'code': error}})
        self.check('invalid', 1, {'error': {'code': 'E401'}})
        self.assertEqual(npm.classify(0, '', *['nestweaver', '1.0.0', INTEGRITY])[0], 'retry')

    def simulate(self, sequence, wait=True, deadline=600, request_duration=0):
        now, sleeps, timeouts, seen = [0], [], [], []
        def clock(): return now[0]
        def sleep(duration):
            sleeps.append(duration)
            now[0] += duration
        def fetch(*args):
            timeouts.append(args[-1])
            now[0] += min(request_duration, args[-1])
            result = sequence[min(len(seen), len(sequence) - 1)]
            seen.append(result)
            return result
        result = npm.verify('nestweaver', '1.0.0', INTEGRITY, deadline, wait,
                            clock, sleep, lambda: 0.5, fetch)
        return result, now[0], sleeps, timeouts, seen

    def test_transient_sequences(self):
        for reason in ['missing-version', 'request-timeout', 'E503', 'empty-response', 'ECONNRESET']:
            result, _, sleeps, timeouts, _ = self.simulate([('retry', reason)] * 3 + [('exact', 'exact')])
            self.assertEqual(result[0], 'exact')
            self.assertEqual(sleeps, [2.25, 4.5, 9])
            self.assertTrue(all(t <= 20 for t in timeouts))

    def test_deadline_and_capped_jitter(self):
        for reason in ['missing-version', 'request-timeout']:
            result, elapsed, sleeps, timeouts, _ = self.simulate([('retry', reason)])
            self.assertEqual(result, ('invalid', f'deadline/{reason}'))
            self.assertEqual(elapsed, 600)
            self.assertEqual(sleeps[:6], [2.25, 4.5, 9, 18, 33.75, 33.75])
            self.assertLessEqual(max(sleeps), 37.5)
            self.assertTrue(all(t <= 20 for t in timeouts))
            self.assertLess(len(timeouts), 30)

    def test_request_timeout_clamped_to_remaining(self):
        result, elapsed, _, timeouts, _ = self.simulate([('retry', 'request-timeout')], deadline=17, request_duration=20)
        self.assertEqual(elapsed, 17)
        self.assertEqual(timeouts, [17])
        self.assertEqual(result, ('invalid', 'deadline/request-timeout'))

    def test_never_retry_mismatch_into_success(self):
        result, _, sleeps, _, seen = self.simulate([('invalid', 'mismatch'), ('exact', 'exact')])
        self.assertEqual(result, ('invalid', 'mismatch'))
        self.assertEqual(sleeps, [])
        self.assertEqual(len(seen), 1)

    def test_precheck_one_probe(self):
        result, _, sleeps, _, seen = self.simulate([('retry', 'missing-version')], wait=False)
        self.assertEqual(result[0], 'retry')
        self.assertEqual(len(seen), 1)
        self.assertEqual(sleeps, [])

    def test_subprocess_timeout_and_registry(self):
        with patch.object(npm.subprocess, 'run', return_value=subprocess.CompletedProcess([], 0, json.dumps(META))) as call:
            self.assertEqual(npm.probe('nestweaver', '1.0.0', INTEGRITY, 7)[0], 'exact')
        self.assertEqual(call.call_args.kwargs['timeout'], 7)
        self.assertIn('--fetch-timeout=7000', call.call_args.args[0])
        self.assertIn('--fetch-retries=0', call.call_args.args[0])
        self.assertIn('--registry=https://registry.npmjs.org', call.call_args.args[0])
        with patch.object(npm.subprocess, 'run', side_effect=subprocess.TimeoutExpired('npm', 20)):
            self.assertEqual(npm.probe('nestweaver', '1.0.0', INTEGRITY, 20), ('retry', 'request-timeout'))


def step_script(name):
    lines = (ROOT / '.github/workflows/release-please.yml').read_text().splitlines()
    start = lines.index('      - name: ' + name)
    start = next(i for i in range(start, len(lines)) if lines[i] == '        run: |') + 1
    end = next((i for i in range(start, len(lines)) if lines[i] and not lines[i].startswith('          ')), len(lines))
    return '\n'.join(line[10:] for line in lines[start:end]) + '\n'


class Workflow(unittest.TestCase):
    def test_source_contracts_and_shell_syntax(self):
        ci_script = step_script('Require successful main CI on the exact release SHA')
        publish = step_script('Publish to npm')
        for script in [ci_script, publish]:
            subprocess.run(['bash', '-n'], input=script, text=True, check=True)
            self.assertIn('policy_root="$GITHUB_WORKSPACE/.release-policy"', script)
            self.assertIn('"$policy_root/scripts/verify-', script)
        self.assertNotIn('RUN_CONCLUSION', ci_script)
        self.assertIn('time.monotonic() + 5100', (ROOT / 'scripts/verify-release-required-ci.py').read_text())
        self.assertIn('sha256sum -c', publish)
        self.assertIn('gh attestation verify', publish)
        self.assertLess(publish.index('gh attestation verify'), publish.index('if registry precheck'))
        self.assertIn('publish_status=$publish_status registry=$final_reason', publish)

    def shell_case(self, sequence, publishes, expected, reason, publish_status=0, recovery=False, budget=600):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            bin_dir = root / 'bin'; bin_dir.mkdir()
            policy = root / '.release-policy' if recovery else root
            (policy / 'scripts').mkdir(parents=True)
            (policy / 'scripts/verify-npm-immutable.py').write_text((ROOT / 'scripts/verify-npm-immutable.py').read_text().replace('time.monotonic() + 600', f'time.monotonic() + {budget}'))
            if recovery:
                (root / 'scripts').mkdir()
                (root / 'scripts/verify-npm-immutable.py').write_text('raise RuntimeError("old policy must not run")')
            package = root / 'npm-package/nestweaver'; package.mkdir(parents=True)
            tarball = package / 'nestweaver-1.0.0.tgz'; tarball.write_bytes(b'fixture attested bytes')
            digest = hashlib.sha256(tarball.read_bytes()).hexdigest()
            (package / (tarball.name + '.sha256')).write_text(digest + '  ' + tarball.name + '\n')
            (root / 'sequence.json').write_text(json.dumps(sequence))
            (root / 'count').write_text('0')
            gh = '#!/bin/sh\nif [ "$1" = api ]; then echo \'{"draft":false,"immutable":true,"tag_name":"v1.0.0","target_commitish":"' + SHA + '"}\'; fi\n'
            (bin_dir / 'gh').write_text(gh)
            (bin_dir / 'npm').write_text('''#!/usr/bin/env python3
import json, os, sys
from pathlib import Path
p = Path(os.environ['FIXTURE'])
if sys.argv[1] == 'publish':
    with (p / 'published').open('a') as f: f.write('publish\\n')
    raise SystemExit(int(os.environ['PUBLISH_STATUS']))
if 'dist.attestations' in sys.argv:
    print('null'); raise SystemExit(0)
i = int((p / 'count').read_text()); (p / 'count').write_text(str(i + 1))
sequence = json.loads((p / 'sequence.json').read_text())
code, response = sequence[min(i, len(sequence)-1)]
print(response)
raise SystemExit(code)
''')
            # A fake timeout executes only our fake npm; no external registry is reachable.
            (bin_dir / 'timeout').write_text('#!/bin/sh\nshift 2\nexec "$@"\n')
            for f in bin_dir.iterdir(): f.chmod(0o755)
            env = os.environ | {'PATH': str(bin_dir) + os.pathsep + os.environ['PATH'],
                'GITHUB_WORKSPACE': str(root), 'RUNNER_TEMP': str(root), 'FIXTURE': str(root),
                'GITHUB_STEP_SUMMARY': str(root / 'summary'), 'GITHUB_REPOSITORY': 'fixture/repo',
                'RELEASE_MODE': 'resume' if recovery else 'publish', 'RELEASE_ID': '1',
                'RELEASE_SHA': SHA, 'RELEASE_TAG': 'v1.0.0', 'PUBLISH_STATUS': str(publish_status),
                'PACKAGES_JSON': json.dumps([META | {'integrity': INTEGRITY, 'filename': tarball.name, 'sha256': digest}])}
            result = subprocess.run(['bash'], input=step_script('Publish to npm'), env=env,
                                    text=True, capture_output=True, timeout=10)
            self.assertEqual(result.returncode, expected, result.stdout + result.stderr)
            actual = (root / 'published').read_text().splitlines() if (root / 'published').exists() else []
            self.assertEqual(len(actual), publishes, result.stdout + result.stderr)
            self.assertIn(reason, (root / 'summary').read_text())

    def test_existing_exact_never_publishes(self):
        self.shell_case([(0, json.dumps(META))], 0, 0, 'publish_status=not-attempted registry=exact')

    def test_absent_then_exact(self):
        self.shell_case([(1, '{"error":{"code":"E404"}}'), (0, json.dumps(META))], 1, 0, 'publish_status=0 registry=exact')

    def test_mismatch_never_publishes(self):
        self.shell_case([(0, json.dumps(META | {'dist.integrity': WRONG}))], 0, 1, 'immutable-integrity-mismatch')

    def test_malformed_never_publishes(self):
        self.shell_case([(0, '{')], 0, 1, 'malformed-json')

    def test_failed_publish_exact_is_success(self):
        self.shell_case([(1, '{"error":{"code":"E404"}}'), (0, json.dumps(META))], 1, 0, 'publish_status=17 registry=exact', publish_status=17)

    def test_mismatch_after_publish_prevents_unsigned_retry(self):
        self.shell_case([(1, '{"error":{"code":"E404"}}'), (0, json.dumps(META | {'dist.integrity': WRONG}))], 1, 1, 'immutable-integrity-mismatch', publish_status=17)

    def test_provenance_fallback_preserved(self):
        absent = (1, '{"error":{"code":"E404"}}')
        self.shell_case([absent, absent, (0, json.dumps(META))], 2, 0, 'publish_status=17 registry=exact', publish_status=17)

    def test_publish_zero_but_missing_until_deadline(self):
        self.shell_case([(1, '{"error":{"code":"E404"}}')], 1, 1,
                        'publish_status=0 registry=deadline/missing-version', budget=1)

    def test_publish_nonzero_but_missing_until_deadline(self):
        self.shell_case([(1, '{"error":{"code":"E404"}}')], 2, 1,
                        'publish_status=17 registry=deadline/missing-version', publish_status=17, budget=1)

    def test_recovery_uses_current_policy(self):
        self.shell_case([(0, json.dumps(META))], 0, 0, 'registry=exact', recovery=True)


if __name__ == '__main__':
    unittest.main(verbosity=2)
