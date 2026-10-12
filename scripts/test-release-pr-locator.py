#!/usr/bin/env python3
"""Execute the release PR locator with offline, sequenced GitHub responses."""
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[1]
BRANCH = 'release-please--branches--main--components--nestweaver'
OLD, HEAD, THIRD = (c * 40 for c in 'abc')


def candidate(sha=HEAD, **changes):
    return dict(number=471, state='open', base=dict(ref='main'),
                head=dict(ref=BRANCH, sha=sha, repo=dict(full_name='o/r', fork=False)),
                user=dict(login='github-actions[bot]')) | changes


FAKE = r'''
import json, os, sys
from pathlib import Path
root = Path(os.environ['FIXTURE_ROOT'])
data = json.loads((root / 'data.json').read_text())
endpoint = sys.argv[2]
kind = 'pulls' if '/pulls/' in endpoint else 'refs'
state = root / (kind + '.count')
i = int(state.read_text()) if state.exists() else 0
state.write_text(str(i + 1))
value = data[kind][min(i, len(data[kind]) - 1)]
print(value['object']['sha'] if '--jq' in sys.argv else json.dumps(value))
'''


class Locator(unittest.TestCase):
    def run_step(self, pulls, refs=None):
        with tempfile.TemporaryDirectory(prefix='nw-locator-') as temp:
            root = Path(temp)
            (root / 'gh').write_text('#!' + sys.executable + '\n' + FAKE)
            (root / 'gh').chmod(0o755)
            (root / 'sleep').write_text('#!/bin/sh\nexit 0\n')
            (root / 'sleep').chmod(0o755)
            (root / 'data.json').write_text(json.dumps(dict(pulls=pulls,
                refs=[dict(object=dict(sha=s)) for s in (refs or [HEAD])])))
            source = (ROOT / '.github/workflows/release-please.yml').read_text().split(
                '      - name: Locate the open release PR\n', 1)[1]
            source = re.split(r'^      - (?:name:|uses:)', source, maxsplit=1, flags=re.M)[0]
            source = source.split('        run: |\n', 1)[1]
            shell = '\n'.join(line[10:] for line in source.splitlines() if line.startswith('          '))
            output = root / 'output'
            env = dict(os.environ, PATH=str(root) + os.pathsep + os.environ['PATH'],
                FIXTURE_ROOT=str(root), GITHUB_OUTPUT=str(output), GITHUB_REPOSITORY='o/r',
                DEFAULT_BRANCH='main', ACTION_PR=json.dumps(dict(number=471, headBranchName=BRANCH)),
                PRS_CREATED='false')
            env.pop('GH_TOKEN', None)
            env.pop('GITHUB_TOKEN', None)
            result = subprocess.run(['bash', '-c', shell], env=env, capture_output=True, text=True, timeout=10)
            return result, output.read_text() if output.exists() else ''

    def test_current_head(self):
        result, output = self.run_step([candidate()])
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn('head_sha=' + HEAD, output)

    def test_stale_pr_converges_to_git_ref(self):
        result, output = self.run_step([candidate(OLD), candidate(OLD), candidate()])
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn('head_sha=' + HEAD, output)
        self.assertNotIn(OLD, output)

    def test_never_converges(self):
        result, output = self.run_step([candidate(OLD)])
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(output, '')

    def test_unexpected_pr_movement(self):
        result, output = self.run_step([candidate(OLD), candidate(THIRD)])
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(output, '')

    def test_git_ref_movement(self):
        result, output = self.run_step([candidate()], [HEAD, THIRD])
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(output, '')

    def test_initial_untrusted_identity(self):
        result, output = self.run_step([candidate(user=dict(login='human'))])
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(output, '')

    def test_invalid_sha(self):
        for pulls, refs in (([candidate('invalid')], [HEAD]), ([candidate()], ['invalid'])):
            with self.subTest(pulls=pulls, refs=refs):
                result, output = self.run_step(pulls, refs)
                self.assertNotEqual(result.returncode, 0)
                self.assertEqual(output, '')

    def test_identity_revalidated(self):
        for initial in (OLD,):
            for bad in (dict(state='closed'), dict(user=dict(login='human')),
                        dict(base=dict(ref='other')), dict(head=dict(ref=BRANCH, sha=HEAD,
                        repo=dict(full_name='fork/r', fork=True)))):
                with self.subTest(initial=initial, bad=bad):
                    result, output = self.run_step([candidate(initial), candidate(**bad)])
                    self.assertNotEqual(result.returncode, 0)
                    self.assertEqual(output, '')


if __name__ == '__main__':
    unittest.main()
