#!/usr/bin/env python3
"""Bind a main-only canary to current same-SHA normal CI runtime evidence."""
import argparse
import importlib.util
import json
import os
from pathlib import Path
import re
import subprocess
from datetime import datetime, timezone

SPEC = importlib.util.spec_from_file_location('release_required', Path(__file__).with_name('verify-release-required-ci.py'))
RELEASE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(RELEASE)
JOB_NAMES = ('Required CI', 'Build & Tests', 'Daemon Integration Tests', 'Standard artifact daemon acceptance')


def observe(repo, sha, run_id, fetch=RELEASE.api):
    if not re.fullmatch('[0-9a-f]{40}', sha) or type(run_id) is not int or run_id <= 0:
        raise ValueError('invalid prerequisite identity')
    def current_main():
        pages = fetch(repo, 'git/ref/heads/main')
        if len(pages) != 1 or pages[0]['object']['sha'] != sha:
            raise ValueError('candidate is not current main')
    current_main()
    path = f'actions/workflows/ci.yml/runs?head_sha={sha}&event=push&per_page=100'
    run = RELEASE.select_run(fetch(repo, path), sha)
    if run is None or run['id'] != run_id or run.get('head_branch') != 'main':
        raise ValueError('prerequisite is not authoritative main CI')
    data = fetch(repo, f"actions/runs/{run_id}/jobs?filter=all&per_page=100")
    state, reason = RELEASE.classify(run, data, sha)
    if state != 'pass':
        raise ValueError(f'Required CI prerequisite is not passing: {reason}')
    jobs = RELEASE.pages(data, 'jobs')
    chosen = []
    for name in JOB_NAMES:
        matches = [job for job in jobs if job['name'] == name and job['run_attempt'] == run['run_attempt']]
        if len(matches) != 1 or matches[0]['status'] != 'completed' or matches[0]['conclusion'] != 'success':
            raise ValueError(f'actual current runtime job must succeed: {name}')
        chosen.append(matches[0])
    latest = RELEASE.select_run(fetch(repo, path), sha)
    if latest is None or (latest['id'], latest['run_attempt']) != (run_id, run['run_attempt']):
        raise ValueError('prerequisite changed during observation')
    current_main()
    return dict(sha=sha, run_id=run_id, run_attempt=run['run_attempt'], workflow_path=run['path'],
                run_url=run.get('html_url'), jobs=chosen, observed_at=datetime.now(timezone.utc).isoformat())


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--sha', required=True)
    parser.add_argument('--run-id', required=True, type=int)
    parser.add_argument('--output', required=True, type=Path)
    parser.add_argument('--previous', type=Path)
    args = parser.parse_args()
    if (os.environ.get('GITHUB_REF') != 'refs/heads/main' or os.environ.get('GITHUB_SHA') != args.sha
            or os.environ.get('GITHUB_WORKFLOW_SHA') != args.sha):
        raise SystemExit('canary workflow ref and SHA must be current main')
    head = subprocess.check_output(['git', 'rev-parse', 'HEAD'], text=True).strip()
    if head != args.sha:
        raise SystemExit('checkout identity mismatch')
    result = observe(os.environ['GITHUB_REPOSITORY'], args.sha, args.run_id)
    if args.previous:
        previous = json.loads(args.previous.read_text())
        for key in ('sha', 'run_id', 'run_attempt', 'workflow_path'):
            if previous[key] != result[key]:
                raise SystemExit('bound prerequisite changed')
        if [j['id'] for j in previous['jobs']] != [j['id'] for j in result['jobs']]:
            raise SystemExit('bound prerequisite jobs changed')
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(result, indent=2) + '\n')
    print(f"Verified current main {result['sha']}: normal CI run {result['run_id']} attempt {result['run_attempt']}; jobs {[j['id'] for j in result['jobs']]} at {result['observed_at']}")


if __name__ == '__main__':
    main()
