#!/usr/bin/env python3
"""Verify synthetic reporter artifacts in addition to actual hosted conclusions."""
import argparse
import hashlib
import json
from pathlib import Path


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--evidence', required=True, type=Path)
    parser.add_argument('--sha', required=True)
    args = parser.parse_args()
    reporter = Path(__file__).with_name('report-mutation-results.py')
    policy_hash = hashlib.sha256(reporter.read_bytes()).hexdigest()
    for case in ('prewarm', 'zero', 'complete', 'malformed', 'outer'):
        matches = list(args.evidence.glob(f'synthetic-{case}-*/synthetic-input.json'))
        assert len(matches) == 1, (case, matches)
        root = matches[0].parent
        binding = json.loads(matches[0].read_text())
        assert binding == dict(synthetic=True, product_coverage=False, case=case,
                               sha=args.sha, reporter_sha256=policy_hash), binding
        result = json.loads((root / 'result.json').read_text())
        assert result['exit_status'] == (0 if case in ('zero', 'complete') else 1), result
        if case == 'prewarm':
            assert result['planned'] is None and not result['complete'] and 'NO SIGNAL' in result['reason'], result
        elif case == 'zero':
            assert result['planned'] == 0 and 'legitimate-zero' in result['reason'], result
        elif case == 'complete':
            assert result['complete'] and result['planned'] == result['accounted'] == 1 and result['caught'] == 1, result
        elif case == 'malformed':
            assert not result['complete'] and 'unverifiable' in result['reason'], result
        elif case == 'outer':
            assert not result['complete'] and result['outer_exhausted'], result
    print('All synthetic artifact contracts verified; no product coverage claimed.')


if __name__ == '__main__':
    main()
