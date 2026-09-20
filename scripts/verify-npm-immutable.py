#!/usr/bin/env python3
"""Read-only immutable npm checks; exit 2 means precheck permits publication."""
import argparse
import base64
import json
import random
import subprocess
import time
from pathlib import Path


def valid_integrity(value):
    try:
        return (isinstance(value, str) and value.startswith('sha512-')
                and len(base64.b64decode(value[7:], validate=True)) == 64)
    except ValueError:
        return False


def classify(code, text, name, version, integrity):
    if code:
        try:
            error = json.loads(text).get('error', {}).get('code', '')
        except (ValueError, AttributeError):
            error = ''
        if error in {'E404', 'ETARGET'}:
            return 'retry', 'missing-version'
        if error in {'E500', 'E502', 'E503', 'E504', 'E429', 'ETIMEDOUT', 'ESOCKETTIMEDOUT',
                     'ECONNRESET', 'ECONNREFUSED', 'ENOTFOUND', 'EAI_AGAIN', 'ENETUNREACH'}:
            return 'retry', error
        return 'invalid', f'probe-error/{error or code}'
    if not text.strip():
        return 'retry', 'empty-response'
    try:
        metadata = json.loads(text)
    except ValueError:
        return 'invalid', 'malformed-json'
    if not isinstance(metadata, dict):
        return 'invalid', 'malformed-metadata'
    if metadata.get('name') != name or metadata.get('version') != version:
        return 'invalid', 'wrong-name-or-version'
    actual = metadata.get('dist.integrity')
    if not valid_integrity(actual):
        return 'invalid', 'missing-or-malformed-integrity'
    if actual != integrity:
        return 'invalid', 'immutable-integrity-mismatch'
    return 'exact', 'exact-immutable-package'


def probe(name, version, integrity, timeout):
    try:
        result = subprocess.run(['npm', 'view', f'{name}@{version}', 'name', 'version',
                                 'dist.integrity', '--json', '--registry=https://registry.npmjs.org',
                                 f'--fetch-timeout={max(1, int(timeout * 1000))}', '--fetch-retries=0'],
                                text=True, capture_output=True, timeout=timeout)
    except subprocess.TimeoutExpired:
        return 'retry', 'request-timeout'
    return classify(result.returncode, result.stdout, name, version, integrity)


def verify(name, version, integrity, deadline, wait, clock=time.monotonic,
           sleep=time.sleep, jitter=random.random, fetch=probe):
    delay = 2
    reason = 'budget-exhausted'
    final_probe = False
    while True:
        remaining = deadline - clock()
        if remaining <= 0:
            return 'invalid', f'deadline/{reason}'
        state, reason = fetch(name, version, integrity, min(20, remaining))
        if state != 'retry' or not wait:
            return state, reason
        remaining = deadline - clock()
        if remaining <= 0:
            return 'invalid', f'deadline/{reason}'
        if final_probe:
            sleep(remaining)
            return 'invalid', f'deadline/{reason}'
        pause = delay * (1 + 0.25 * jitter())
        if pause >= remaining:
            # Reserve one final bounded probe just before expiry.
            pause = max(0, remaining - min(1, remaining))
            final_probe = True
        sleep(pause)
        delay = min(30, delay * 2)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('mode', choices=['precheck', 'probe', 'wait', 'remaining'])
    for arg in ['name', 'version', 'integrity', 'state']:
        parser.add_argument(f'--{arg}', required=True)
    args = parser.parse_args()
    if not valid_integrity(args.integrity):
        parser.error('expected integrity must be SHA-512 SRI')
    path = Path(args.state)
    identity = [args.name, args.version, args.integrity]
    if args.mode == 'precheck':
        saved = {'identity': identity, 'deadline': time.monotonic() + 600}
    else:
        saved = json.loads(path.read_text())
        if saved['identity'] != identity or saved.get('precheck_state') != 'retry':
            parser.error('state is not an authorized retry for this immutable package')
    if args.mode == 'remaining':
        remaining = saved['deadline'] - time.monotonic()
        if remaining <= 0:
            return 1
        print(remaining)
        return 0
    state, reason = verify(*identity, saved['deadline'], args.mode == 'wait')
    if args.mode == 'precheck':
        saved['precheck_state'] = state
        path.write_text(json.dumps(saved))
    print(json.dumps({'state': state, 'final_reason': reason}))
    return 0 if state == 'exact' else 2 if state == 'retry' else 1


if __name__ == '__main__':
    raise SystemExit(main())
