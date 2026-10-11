#!/usr/bin/env python3
"""Execute the changelog workflow step against an offline GitHub CLI fixture."""
import base64
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[1]
WORKFLOW = ROOT / '.github/workflows/release-please.yml'


def workflow_step():
    source = WORKFLOW.read_text().split(
        '      - name: Deduplicate CHANGELOG.md on the release PR branch (nw-096)\n', 1)[1]
    source = re.split(r'^      - (?:name:|uses:)', source, maxsplit=1, flags=re.M)[0]
    source = source.split('        run: |\n', 1)[1]
    return '\n'.join(line[10:] for line in source.splitlines() if line.startswith('          ')) + '\n'


def fixture():
    def bullet(description, digit):
        return f'* {description} ([{digit * 7}](https://github.com/o/r/commit/{digit * 40}))\n'
    prefix = '# Changelog\n\n## [2.0.0](https://x/compare/v1...v2) (2026-10-11)\n\n### Fixes\n\n'
    kept = bullet('same description', '1') + '  kept continuation\n\n' + bullet('other description', '2') + '\n'
    removed = bullet('same description', '3') + '  removed continuation\n\n'
    # Keep historical UTF-8 text and trailing blank lines exactly intact.
    history = '## [1.0.0](https://x/compare/v0...v1) (2026-01-01)\n' + ('Historical café — 修正\n' * 20000) + '\n\n'
    return (prefix + kept + removed + history).encode(), (prefix + kept + history).encode()


FAKE_GH = r'''
import json, os, sys
from pathlib import Path
root = Path(os.environ['FIXTURE_ROOT'])
args = sys.argv[1:]
with (root / 'calls.jsonl').open('a') as out:
    out.write(json.dumps(args) + '\n')
data = json.loads((root / 'data.json').read_text())
if args[:2] == ['api', '--method']:
    if args[2] != 'PUT' or '--input' not in args or max(map(len, args)) > 4096:
        raise SystemExit('upload must use a bounded file-body invocation')
    body = Path(args[args.index('--input') + 1]).read_bytes()
    (root / 'uploaded.json').write_bytes(body)
    if data.get('upload_failure'):
        print('HTTP 409: changed blob SHA', file=sys.stderr)
        raise SystemExit(1)
elif args[0] == 'api':
    if data.get('read_failure'):
        print('HTTP ' + str(data['read_failure']), file=sys.stderr)
        raise SystemExit(1)
    print(json.dumps(data['contents']))
elif args[:2] == ['pr', 'list']:
    print(json.dumps(data['pulls']))
else:
    raise SystemExit('unexpected GitHub invocation')
'''


class Upload(unittest.TestCase):
    def run_step(self, content=None, pulls=None, read_failure=None, upload_failure=False, dedupe=None):
        temp = tempfile.TemporaryDirectory(prefix='nw-upload-test-')
        self.addCleanup(temp.cleanup)
        root = Path(temp.name)
        scripts = root / 'scripts'
        scripts.mkdir()
        if dedupe is None:
            shutil.copyfile(ROOT / 'scripts/dedupe-changelog-entries.sh', scripts / 'dedupe-changelog-entries.sh')
        else:
            (scripts / 'dedupe-changelog-entries.sh').write_text(dedupe)
        (root / 'gh').write_text('#!' + sys.executable + '\n' + FAKE_GH)
        (root / 'gh').chmod(0o755)
        content = content if content is not None else fixture()[0]
        pulls = pulls if pulls is not None else [dict(number=1, baseRefName='main',
            author=dict(login='app/github-actions'), isCrossRepository=False)]
        (root / 'data.json').write_text(json.dumps(dict(contents=dict(sha='a' * 40,
            content=base64.b64encode(content).decode()), pulls=pulls,
            read_failure=read_failure, upload_failure=upload_failure)))
        env = dict(os.environ, PATH=str(root) + os.pathsep + os.environ['PATH'],
                   FIXTURE_ROOT=str(root), DEFAULT_BRANCH='main', GITHUB_REPOSITORY='test-owner/test-repo')
        env.pop('GH_TOKEN', None)
        env.pop('GITHUB_TOKEN', None)
        result = subprocess.run(['bash', '-c', workflow_step()], cwd=root, env=env,
                                capture_output=True, text=True, timeout=30)
        return root, result

    def test_large_body_keeps_exact_content_and_optimistic_sha(self):
        original, expected = fixture()
        self.assertGreater(len(base64.b64encode(original)), 131072)
        root, result = self.run_step()
        self.assertEqual(result.returncode, 0, result.stderr)
        payload = json.loads((root / 'uploaded.json').read_text())
        self.assertEqual(base64.b64decode(payload['content']), expected)
        self.assertEqual(payload['sha'], 'a' * 40)
        self.assertEqual(payload['branch'], 'release-please--branches--main--components--nestweaver')
        self.assertEqual(payload['message'], 'fix(release): deduplicate merge-commit changelog entries (nw-096)')
        calls = [json.loads(line) for line in (root / 'calls.jsonl').read_text().splitlines()]
        self.assertLess(max(len(arg) for call in calls for arg in call), 4096)
        self.assertFalse((root / 'CHANGELOG.md.upload.json').exists())

    def test_legacy_bot_author_is_still_accepted(self):
        root, result = self.run_step(pulls=[dict(number=1, baseRefName='main',
            author=dict(login='github-actions[bot]'), isCrossRepository=False)])
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertTrue((root / 'uploaded.json').exists())

    def test_unchanged_content_never_uploads(self):
        root, result = self.run_step(content=fixture()[1])
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertFalse((root / 'uploaded.json').exists())

    def test_untrusted_or_ambiguous_pr_never_uploads(self):
        trusted = dict(number=1, baseRefName='main', author=dict(login='app/github-actions'), isCrossRepository=False)
        for pulls in ([], [trusted, trusted], [dict(trusted, baseRefName='other')],
                      [dict(trusted, isCrossRepository=True)], [dict(trusted, author=dict(login='someone'))]):
            root, result = self.run_step(pulls=pulls)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertFalse((root / 'uploaded.json').exists())

    def test_missing_branch_is_allowed_but_read_failure_is_not(self):
        for status, expected in [(404, 0), (403, 1)]:
            root, result = self.run_step(read_failure=status)
            self.assertEqual(result.returncode, expected, result.stderr)
            self.assertFalse((root / 'uploaded.json').exists())

    def test_empty_output_and_excessive_removal_are_refused(self):
        for dedupe in ('#!/bin/bash\nexit 0\n', "#!/bin/bash\nprintf '# Changelog\\n'\n"):
            root, result = self.run_step(dedupe=dedupe)
            self.assertNotEqual(result.returncode, 0)
            self.assertFalse((root / 'uploaded.json').exists())
        content = fixture()[0] + b''.join(
            f'* older entry {i} ([aaaaaaa](https://github.com/o/r/commit/{"a" * 40}))\n'.encode()
            for i in range(13))
        root, result = self.run_step(content=content, dedupe='#!/bin/bash\nhead -n 8 "$1"\n')
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('outside the safe bound', result.stderr)
        self.assertFalse((root / 'uploaded.json').exists())

    def test_stale_sha_failure_propagates_without_retry(self):
        root, result = self.run_step(upload_failure=True)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('HTTP 409', result.stderr)
        calls = [json.loads(line) for line in (root / 'calls.jsonl').read_text().splitlines()]
        self.assertEqual(sum(call[:2] == ['api', '--method'] for call in calls), 1)


if __name__ == '__main__':
    unittest.main(verbosity=2)
