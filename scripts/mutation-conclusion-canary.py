#!/usr/bin/env python3
"""Synthetic outcome-policy checks only: no product build or mutation coverage.

Each invocation calls the production reporter as a separate process. Hosted jobs
must expose expected failures as actual failed job conclusions, without continue-
on-error. A separate verdict job checks those actual conclusions.
"""
import argparse
import hashlib
import json
from pathlib import Path
import subprocess
import sys

ROOT = Path(__file__).resolve().parents[1]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--case', choices=['prewarm', 'zero', 'complete', 'malformed', 'outer'], required=True)
    parser.add_argument('--evidence', type=Path, required=True)
    parser.add_argument('--sha', required=True)
    args = parser.parse_args()
    if subprocess.check_output(['git', 'rev-parse', 'HEAD'], cwd=ROOT, text=True).strip() != args.sha:
        parser.error('checked-out source differs from frozen candidate SHA')
    root = args.evidence
    root.mkdir(parents=True, exist_ok=True)
    reporter = ROOT / 'scripts/report-mutation-results.py'
    (root / 'synthetic-input.json').write_text(json.dumps(dict(
        synthetic=True, product_coverage=False, case=args.case, sha=args.sha,
        reporter_sha256=hashlib.sha256(reporter.read_bytes()).hexdigest()), indent=2))
    command = [sys.executable, str(reporter), '--evidence', str(root)]
    if args.case == 'prewarm':
        command.append('--prewarm-skipped')
    else:
        mutants = root / 'mutants.out'
        mutants.mkdir()
        plan = [] if args.case == 'zero' else [dict(name='synthetic:1:1: replace ready with false',
            package='synthetic', file='src/lib.rs', replacement='false', genre='FnValue',
            span={'start': {'line': 1, 'column': 1}})]
        (root / 'status.json').write_text(json.dumps(dict(status=124 if args.case == 'outer' else 0,
            truncated=args.case == 'outer', discovery_status=0, no_packages=False)))
        (root / 'discovery.json').write_text(json.dumps(plan))
        (root / 'discovery.log').write_text('No mutants to filter' if args.case == 'zero' else 'synthetic discovery')
        (root / 'console.log').write_text('No mutants to filter' if args.case == 'zero' else 'synthetic result')
        (mutants / 'mutants.json').write_text(json.dumps(plan))
        outcomes = [] if not plan else [dict(scenario={'Mutant': plan[0]}, summary='CaughtMutant',
            phase_results=[dict(phase='Test', argv=['synthetic', 'never-executed'], duration=0.1,
                               process_status={'Failure': 101})])]
        report = dict(caught=len(plan), missed=0, timeout=0, unviable=0, total_mutants=len(plan), outcomes=outcomes)
        (mutants / 'outcomes.json').write_text('{' if args.case == 'malformed' else json.dumps(report))
    result = subprocess.run(command)
    expected = 0 if args.case in ('zero', 'complete') else 1
    if result.returncode != expected:
        print(f'Policy regression: expected {expected}, received {result.returncode}', file=sys.stderr)
        # In a negative job a policy regression must unexpectedly succeed so
        # the independent verdict fails; controls fail directly.
        return 0 if expected else 1
    return expected


if __name__ == '__main__':
    raise SystemExit(main())
