#!/usr/bin/env python3
"""Classify pinned cargo-mutants 27.1.0 evidence without inventing coverage."""
import argparse
import json
import os
from pathlib import Path

ZERO_MESSAGES = ('Diff file is empty', 'Diff changes no Rust source files', 'No mutants to filter')
VERDICTS = {'CaughtMutant': 'caught', 'MissedMutant': 'survived', 'Timeout': 'timed_out', 'Unviable': 'unviable'}
COUNTERS = {'caught': 'caught', 'missed': 'survived', 'timeout': 'timed_out', 'unviable': 'unviable'}


def read(path):
    return json.loads(Path(path).read_text())


def identity(mutant):
    if not isinstance(mutant, dict) or any(not isinstance(mutant.get(k), str) or not mutant[k]
                                          for k in ('name', 'package', 'file', 'genre')):
        raise ValueError('invalid mutant identity')
    # Discovery adds a diff field; the scenario representation deliberately omits it.
    return json.dumps({k: v for k, v in mutant.items() if k != 'diff'}, sort_keys=True)


def plan_ids(plan):
    if not isinstance(plan, list):
        raise ValueError('malformed planned mutant list')
    ids = [identity(m) for m in plan]
    names = [m['name'] for m in plan]
    if len(set(ids)) != len(ids) or len(set(names)) != len(names):
        raise ValueError('duplicate planned mutant identities')
    return ids


def integer(value):
    if type(value) is not int or value < 0:
        raise ValueError('counts must be nonnegative integers')
    return value


def phase_verdict(phases):
    if not isinstance(phases, list) or not phases:
        raise ValueError('missing authentic phase outcomes')
    for phase in phases:
        if phase['phase'] not in ('Build', 'Check', 'Test') or not isinstance(phase['argv'], list):
            raise ValueError('malformed phase evidence')
    def failure(value):
        return isinstance(value, dict) and set(value) == {'Failure'} and type(value['Failure']) is int and value['Failure'] != 0
    if any(p['phase'] != 'Test' and failure(p['process_status']) for p in phases):
        return 'Unviable'
    if any(p['process_status'] == 'Timeout' for p in phases):
        return 'Timeout'
    last = phases[-1]
    if last['phase'] == 'Test' and failure(last['process_status']):
        return 'CaughtMutant'
    if last['phase'] == 'Test' and last['process_status'] == 'Success':
        return 'MissedMutant'
    raise ValueError('no terminal mutation verdict in phase outcomes')


def classify(directory, state, discovery=None, console=''):
    root = Path(directory)
    result = dict(planned=None, accounted=None, outstanding=None, caught=0, survived=0,
                  timed_out=0, unviable=0, complete=False, outer_exhausted=False,
                  exit_status=1, reason='unverifiable report', verdicts=[])
    try:
        status = state['status']
        if type(status) is not int or type(state.get('truncated')) is not bool:
            raise ValueError('invalid driver status')
        result['status'] = status
        result['outer_exhausted'] = status in (124, 137) or state['truncated']
        if state.get('no_packages') is True and status == 0 and not result['outer_exhausted']:
            result.update(planned=0, accounted=0, outstanding=0, exit_status=0,
                          reason='legitimate-zero: no Rust package in diff; no coverage measured')
            return result
        plan_path = root / 'mutants.json'
        if not plan_path.exists() and discovery == [] and any(m in console for m in ZERO_MESSAGES):
            plan = []
        else:
            plan = read(plan_path)
        ids = plan_ids(plan)
        result['planned'] = len(ids)
        if discovery is not None and plan_ids(discovery) != ids:
            raise ValueError('discovery and executed plan differ')
        if not ids:
            if status != 0 or result['outer_exhausted'] or discovery != [] or not any(m in console for m in ZERO_MESSAGES):
                raise ValueError('zero plan lacks recognized successful zero discovery/result')
            if (root / 'outcomes.json').exists():
                zero_report = read(root / 'outcomes.json')
                if any(integer(zero_report[key]) != 0 for key in (*COUNTERS, 'total_mutants')) or zero_report['outcomes'] != []:
                    raise ValueError('zero discovery contradicts outcome report')
            result.update(accounted=0, outstanding=0, exit_status=0,
                          reason='legitimate-zero: diff generated no mutants; no coverage measured')
            return result
        report_path = root / 'outcomes.json'
        seen = set()
        if report_path.exists():
            report = read(report_path)  # Torn/malformed JSON fails closed; never replace it with invented records.
            records = report['outcomes']
            if not isinstance(records, list):
                raise ValueError('invalid outcome records')
            for record in records:
                scenario = record['scenario']
                if scenario == 'Baseline':
                    if record['summary'] != 'Success':
                        raise ValueError('baseline did not succeed')
                    continue
                mutant = scenario['Mutant']
                key = identity(mutant)
                if key not in ids or key in seen:
                    raise ValueError('unrelated or duplicate outcome identity')
                seen.add(key)
                if phase_verdict(record['phase_results']) != record['summary']:
                    raise ValueError('summary contradicts authentic phase outcomes')
                verdict = VERDICTS[record['summary']]
                result[verdict] += 1
                result['verdicts'].append({'name': mutant['name'], 'verdict': verdict,
                                           'phase_results': record['phase_results']})
            for source, dest in COUNTERS.items():
                if integer(report[source]) != result[dest]:
                    raise ValueError('counters disagree with outcome records')
            if integer(report['total_mutants']) != len(seen):
                raise ValueError('total_mutants disagrees with outcome records')
        else:
            # Interruption before the first outcome write: v27 creates all four
            # verdict files up front. Require all, and validate every line against
            # the authentic plan. File absence alone is never zero accounting.
            names = {m['name'] for m in plan}
            for source, dest in COUNTERS.items():
                for name in (root / (source + '.txt')).read_text().splitlines():
                    if name not in names or name in seen:
                        raise ValueError('unrelated or duplicate verdict-file identity')
                    seen.add(name)
                    result[dest] += 1
                    result['verdicts'].append({'name': name, 'verdict': dest, 'phase_results': None})
        result['accounted'] = sum(result[k] for k in COUNTERS.values())
        if result['accounted'] > result['planned']:
            raise ValueError('accounted exceeds planned')
        result['outstanding'] = result['planned'] - result['accounted']
        if result['outer_exhausted']:
            result['reason'] = 'outer budget exhausted; completion invalidated regardless of recorded timeouts'
        elif status not in (0, 2, 3):
            result['reason'] = 'untrustworthy cargo-mutants exit status'
        elif result['outstanding']:
            result['reason'] = 'incomplete mutant accounting'
        else:
            expected_status = 3 if result['timed_out'] else 2 if result['survived'] else 0
            if status != expected_status:
                raise ValueError('tool exit status contradicts terminal verdicts')
            result.update(complete=True, exit_status=0)
            result['reason'] = ('complete accounting with mutation timeouts' if result['timed_out'] else
                                'complete accounting with surviving mutants' if result['survived'] else
                                'complete mutant accounting')
    except (ValueError, OSError, KeyError, TypeError) as exc:
        prefix = 'outer budget exhausted; ' if result['outer_exhausted'] else ''
        result['reason'] = prefix + f'unverifiable report: {exc}'
    return result


def render(result):
    counts = ', '.join(f'{key}={result[key]}' for key in
                       ('planned', 'accounted', 'outstanding', 'caught', 'survived', 'timed_out', 'unviable'))
    text = f"## Mutation testing (advisory)\n\n{result['reason']}.\n\n{counts}.\n\n"
    text += 'Accounting includes build failures and timeouts; it does not assert that every mutant reached tests.\n'
    survivors = [v['name'] for v in result['verdicts'] if v['verdict'] == 'survived']
    if survivors:
        text += '\nSurviving mutants:\n\n```text\n' + '\n'.join(survivors) + '\n```\n'
    return text


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--evidence', required=True, type=Path)
    parser.add_argument('--prewarm-skipped', action='store_true')
    args = parser.parse_args()
    args.evidence.mkdir(parents=True, exist_ok=True)
    if args.prewarm_skipped:
        result = dict(planned=None, accounted=None, outstanding=None, caught=0, survived=0,
                      timed_out=0, unviable=0, complete=False, outer_exhausted=False, exit_status=0,
                      reason='SKIPPED: prewarm budget exhausted; no mutation coverage measured', verdicts=[])
    else:
        try:
            state = read(args.evidence / 'status.json')
            raw = (args.evidence / 'discovery.json').read_text()
            discovery_log = (args.evidence / 'discovery.log').read_text()
            discovery = json.loads(raw) if raw.strip() else ([] if state.get('discovery_status') == 0 and
                         any(m in discovery_log for m in ZERO_MESSAGES) else None)
            result = classify(args.evidence / 'mutants.out', state, discovery=discovery,
                              console=(args.evidence / 'console.log').read_text())
        except (OSError, ValueError, KeyError, TypeError) as exc:
            result = classify(args.evidence / 'mutants.out', {})
            result['reason'] = f'unverifiable driver evidence: {exc}'
    (args.evidence / 'result.json').write_text(json.dumps(result, indent=2) + '\n')
    summary = render(result)
    print(summary)
    if os.environ.get('GITHUB_STEP_SUMMARY'):
        with open(os.environ['GITHUB_STEP_SUMMARY'], 'a') as stream:
            stream.write(summary)
    return result['exit_status']


if __name__ == '__main__':
    raise SystemExit(main())
