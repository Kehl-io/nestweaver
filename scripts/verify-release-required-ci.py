#!/usr/bin/env python3
"""Gate publication on the newest exact-SHA push run's current Required CI."""
import argparse
import json
import re
import subprocess
import time

PENDING = {'queued', 'in_progress', 'waiting', 'requested', 'pending'}


def pages(data, key):
    if not isinstance(data, list) or not data:
        raise ValueError('missing paginated response')
    items = []
    counts = []
    for page in data:
        if not isinstance(page, dict) or not isinstance(page.get(key), list):
            raise ValueError('malformed page')
        count = page.get('total_count')
        if type(count) is not int or count < 0:
            raise ValueError('invalid total_count')
        counts.append(count)
        items.extend(page[key])
    if len(set(counts)) != 1 or counts[0] != len(items):
        raise ValueError('truncated or changing pagination')
    ids = [item['id'] for item in items]
    if any(type(i) is not int or i <= 0 for i in ids) or len(set(ids)) != len(ids):
        raise ValueError('invalid or duplicate ids')
    return items


def select_run(data, sha):
    runs = pages(data, 'workflow_runs')
    matches = [r for r in runs if r.get('head_sha') == sha
               and r.get('event') == 'push'
               and r.get('path', '').split('@')[0] == '.github/workflows/ci.yml']
    return max(matches, key=lambda r: r['id']) if matches else None


def classify(run, data, sha):
    attempt = run.get('run_attempt')
    if type(attempt) is not int or attempt < 1 or run.get('status') not in PENDING | {'completed'}:
        raise ValueError('invalid run attempt/status')
    jobs = pages(data, 'jobs')
    for job in jobs:
        if job.get('head_sha') != sha:
            raise ValueError('wrong job SHA')
        n = job.get('run_attempt')
        if type(n) is not int or not 1 <= n <= attempt:
            raise ValueError('invalid/newer job attempt')
    required = [j for j in jobs if j.get('name') == 'Required CI']
    if len({j['run_attempt'] for j in required}) != len(required):
        raise ValueError('duplicate Required CI attempt')
    current = [j for j in required if j['run_attempt'] == attempt]
    if not current:
        return ('fail' if run['status'] == 'completed' else 'pending', 'missing current Required CI')
    job = current[0]
    reason = f"job={job['id']} status={job.get('status')} conclusion={job.get('conclusion')} url={job.get('html_url')}"
    if job.get('status') == 'completed':
        return ('pass' if job.get('conclusion') == 'success' else 'fail', reason)
    if job.get('status') in PENDING and job.get('conclusion') is None and run['status'] != 'completed':
        return 'pending', reason
    return 'fail', reason


def api(repo, path):
    result = subprocess.run(['gh', 'api', '--paginate', '--slurp', f'repos/{repo}/{path}'],
                            check=True, capture_output=True, text=True, timeout=60)
    return json.loads(result.stdout)


def poll(repo, sha, fetch=api):
    path = f'actions/workflows/ci.yml/runs?head_sha={sha}&event=push&per_page=100'
    run = select_run(fetch(repo, path), sha)
    if run is None:
        return 'pending', 'no exact-SHA push CI run'
    jobs = fetch(repo, f"actions/runs/{run['id']}/jobs?filter=all&per_page=100")
    result, reason = classify(run, jobs, sha)
    # Re-read after jobs to catch a rerun or newer run that started during fetch.
    latest = select_run(fetch(repo, path), sha)
    if latest is None or (latest['id'], latest['run_attempt']) != (run['id'], run['run_attempt']):
        return 'pending', 'CI run/attempt changed while reading jobs'
    return result, f"run={run['id']} attempt={run['run_attempt']} url={run.get('html_url')} {reason}"


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--repo', required=True)
    parser.add_argument('--sha', required=True)
    args = parser.parse_args()
    if not re.fullmatch('[0-9a-f]{40}', args.sha):
        parser.error('SHA must be 40 lowercase hexadecimal characters')
    deadline = time.monotonic() + 5100  # 85 minutes inside the 90-minute step.
    reason = 'no observation'
    while time.monotonic() < deadline:
        try:
            state, reason = poll(args.repo, args.sha)
        except (ValueError, KeyError, TypeError, subprocess.SubprocessError) as exc:
            print(f'FAIL: unreadable CI evidence: {exc}', flush=True)
            return 1
        print(f'{state}: {reason}', flush=True)
        if state != 'pending':
            return 0 if state == 'pass' else 1
        time.sleep(min(15, max(0, deadline - time.monotonic())))
    print(f'FAIL: deadline: {reason}')
    return 1


if __name__ == '__main__':
    raise SystemExit(main())
