#!/usr/bin/env python3
"""Locate the OpenSSL install reported by this Cargo build, independent of layout."""
import json
from pathlib import Path
import re
import sys


def source(events):
    outputs = {
        Path(event['out_dir']).resolve() / 'openssl-build' / 'install'
        for event in events
        if event.get('reason') == 'build-script-executed'
        and re.search(r'#openssl-sys@[^#]+$', event.get('package_id', ''))
    }
    if len(outputs) != 1:
        raise ValueError(f'expected one openssl-sys build output, found {len(outputs)}')
    install = outputs.pop()
    for name in ('lib/libssl.a', 'lib/libcrypto.a', 'include/openssl/ssl.h',
                 'include/openssl/opensslconf.h', 'lib/pkgconfig/openssl.pc'):
        if not (install / name).is_file():
            raise ValueError(f'missing built OpenSSL file: {install / name}')
    return install


if __name__ == '__main__':
    try:
        with open(sys.argv[1]) as stream:
            print(source(json.loads(line) for line in stream if line.strip()))
    except (ValueError, KeyError, OSError) as error:
        sys.exit(f'OpenSSL release preparation failed: {error}')
