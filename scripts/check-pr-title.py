#!/usr/bin/env python3
"""Keep merge titles out of Release Please's conventional change population."""
import json
import os
from pathlib import Path
import re
import subprocess

CONVENTIONAL = re.compile(r"^[A-Za-z][A-Za-z0-9_-]*(?:\([^\r\n)]*\))?!?:\s+\S")


def check(event, head_subject=None):
    pr = event.get('pull_request')
    if not pr:
        return
    title = pr.get('title', '')
    branch = pr.get('head', {}).get('ref', '')
    # Generated release PRs intentionally carry one release subject.
    if branch.startswith('release-please--') and title.startswith('chore(main): release '):
        return
    # A one-commit PR with the exact same subject is already handled by the
    # existing exact-description dedupe. Keep dependency-bot PRs unchanged.
    if pr.get('commits') == 1 and title == head_subject:
        return
    if CONVENTIONAL.match(title):
        raise ValueError('Use a plain-language PR title without a conventional-commit prefix. '
                         'Keep conventional subjects on commits so the merge does not add a synthetic changelog entry.')


if __name__ == '__main__':
    try:
        event = json.loads(Path(os.environ['GITHUB_EVENT_PATH']).read_text())
        subject = None
        if event.get('pull_request', {}).get('commits') == 1:
            subject = subprocess.check_output(['git', 'log', '-1', '--format=%s'], text=True).strip()
        check(event, subject)
    except ValueError as error:
        raise SystemExit(str(error)) from error
