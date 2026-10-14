#!/usr/bin/env python3
"""Execute the release lockfile publication step with offline GitHub responses."""
import base64
import hashlib
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
BRANCH = 'release-please--branches--main--components--nestweaver'
HEAD, NEW, THIRD, BLOB = (c * 40 for c in 'abcd')
LOCK = b'# synthetic Cargo.lock\nversion = 4\n'

# Answers `gh` the way the step calls it, records every call, and serves the
# published commit as the PR head once the ref has been PATCHed.
FAKE = r'''
import json, os, sys
from pathlib import Path
root = Path(os.environ['FIXTURE_ROOT'])
data = json.loads((root / 'data.json').read_text())
args = sys.argv[1:]
stdin = sys.stdin.read() if '--input' in args else ''
with (root / 'calls.jsonl').open('a') as log:
    log.write(json.dumps(dict(args=args, stdin=stdin)) + '\n')
if args[0] == 'pr':
    sys.exit(0)
method = args[args.index('--method') + 1] if '--method' in args else 'GET'
endpoint = next(a for a in args[1:] if a.startswith('repos/'))
jq = args[args.index('--jq') + 1] if '--jq' in args else None
patched = (root / 'patched').exists()
if method == 'PATCH':
    (root / 'patched').write_text(json.loads(stdin)['sha'])
    print('{}')
elif method == 'POST':
    kind = endpoint.rsplit('/', 1)[1]
    print({'blobs': 'e' * 40, 'trees': 'f' * 40, 'commits': data['new']}[kind])
elif '/pulls/' in endpoint:
    pr = data['pr']
    if patched:
        pr = dict(pr, head=dict(pr['head'], sha=(root / 'patched').read_text()))
    if jq == '.head.sha':
        print(pr['head']['sha'])
    elif jq == '.draft':
        print(json.dumps(pr['draft']))
    else:
        print(json.dumps(pr))
elif '/contents/Cargo.lock' in endpoint:
    print(json.dumps(dict(sha=data['blob'])))
elif '/git/ref/heads/' in endpoint:
    print(data['branch_head'])
elif '/git/commits/' in endpoint:
    print('c' * 40)
else:
    sys.exit('unexpected gh call: ' + ' '.join(args))
'''

# The step targets ubuntu-latest and uses GNU spellings. Where the host's
# tools are BSD (macOS), stand in for exactly the calls the step makes.
DARWIN_SHIMS = {
    'find': '#!/bin/sh\nexec gfind "$@"\n',
    'stat': '#!/bin/sh\nexec gstat "$@"\n',
    'sha256sum': '#!/bin/sh\nexec shasum -a 256 "$@"\n',
    'base64': '#!/bin/sh\n[ "$1" = -w ] && shift 2\nexec /usr/bin/base64 -i "$1"\n',
}


def pull(sha=HEAD, **changes):
    return dict(number=480, state='open', draft=False, base=dict(ref='main'),
                head=dict(ref=BRANCH, sha=sha, repo=dict(full_name='o/r', fork=False)),
                user=dict(login='github-actions[bot]')) | changes


def step_shell():
    source = (ROOT / '.github/workflows/release-please.yml').read_text().split(
        '      - name: Revalidate identity and publish Cargo.lock with an exact-head lease\n', 1)[1]
    source = re.split(r'^  [a-z][a-z-]*:$|^      - (?:name:|uses:)', source,
                      maxsplit=1, flags=re.M)[0]
    source = source.split('        run: |\n', 1)[1]
    return '\n'.join(line[10:] for line in source.splitlines())


class LockfilePublication(unittest.TestCase):
    def run_step(self, changed, pr=None, branch_head=HEAD):
        with tempfile.TemporaryDirectory(prefix='nw-lockfile-') as temp:
            root = Path(temp)
            work = root / 'work'
            (work / 'lockfile-artifact').mkdir(parents=True)
            (work / 'lockfile-artifact/Cargo.lock').write_bytes(LOCK)
            (work / 'lockfile-artifact/lockfile-state.json').write_text(json.dumps(dict(
                head_sha=HEAD, original_blob=BLOB, changed=changed,
                sha256=hashlib.sha256(LOCK).hexdigest())))
            bins = root / 'bin'
            bins.mkdir()
            tools = dict(gh='#!' + sys.executable + '\n' + FAKE, sleep='#!/bin/sh\nexit 0\n')
            if sys.platform == 'darwin':
                tools |= DARWIN_SHIMS
            for name, body in tools.items():
                (bins / name).write_text(body)
                (bins / name).chmod(0o755)
            (root / 'data.json').write_text(json.dumps(dict(
                pr=pr or pull(), blob=BLOB, branch_head=branch_head, new=NEW)))
            env = dict(os.environ, PATH=str(bins) + os.pathsep + os.environ['PATH'],
                       FIXTURE_ROOT=str(root), GITHUB_REPOSITORY='o/r',
                       RELEASE_PR_BRANCH=BRANCH, RELEASE_PR_HEAD_SHA=HEAD,
                       RELEASE_PR_NUMBER='480')
            env.pop('GH_TOKEN', None)
            env.pop('GITHUB_TOKEN', None)
            result = subprocess.run(['bash', '-c', step_shell()], cwd=work, env=env,
                                    capture_output=True, text=True, timeout=30)
            calls_file = root / 'calls.jsonl'
            calls = [json.loads(line) for line in calls_file.read_text().splitlines()] \
                if calls_file.exists() else []
            return result, calls

    @staticmethod
    def writes(calls):
        return [c for c in calls if '--method' in c['args'] or c['args'][0] == 'pr']

    def test_an_already_synchronized_lockfile_passes_without_writing(self):
        # nw-784: `jq -e` exits 1 on `false`, so this path died under
        # `set -e` with no message on every main push after the sync.
        result, calls = self.run_step(changed=False)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.writes(calls), [])

    def test_a_changed_lockfile_is_published_onto_the_leased_head(self):
        result, calls = self.run_step(changed=True)
        self.assertEqual(result.returncode, 0, result.stderr)
        writes = self.writes(calls)
        self.assertEqual([c['args'][3].rsplit('/', 1)[-1] for c in writes],
                         ['blobs', 'trees', 'commits', BRANCH])
        blob = json.loads(writes[0]['stdin'])
        self.assertEqual((blob['encoding'], base64.b64decode(blob['content'])),
                         ('base64', LOCK), 'the blob must carry the artifact lockfile')
        tree = json.loads(writes[1]['stdin'])
        self.assertEqual([entry['path'] for entry in tree['tree']], ['Cargo.lock'])
        self.assertEqual(json.loads(writes[2]['stdin'])['parents'], [HEAD])
        self.assertEqual(json.loads(writes[3]['stdin']), dict(sha=NEW, force=False))

    def test_the_lease_still_refuses_a_moved_pull_request(self):
        for changed in (False, True):
            with self.subTest(changed=changed):
                result, calls = self.run_step(changed=changed, pr=pull(THIRD))
                self.assertNotEqual(result.returncode, 0)
                self.assertIn('publish-lockfile [pr-identity]', result.stderr)
                self.assertEqual(self.writes(calls), [])

    def test_the_lease_still_refuses_a_moved_branch(self):
        result, calls = self.run_step(changed=True, branch_head=THIRD)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('publish-lockfile [branch-head-lease]', result.stderr)
        self.assertEqual(self.writes(calls), [])


if __name__ == '__main__':
    if sys.platform == 'darwin' and not (shutil.which('gfind') and shutil.which('gstat')):
        sys.exit('macOS needs GNU findutils and coreutils (gfind, gstat) to run this step')
    unittest.main()
