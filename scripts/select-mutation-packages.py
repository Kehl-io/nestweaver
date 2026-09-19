#!/usr/bin/env python3
"""Select packages once from the same diff's file inventory."""
import re
import sys
from pathlib import Path


def select(files):
    packages = set()
    for name in files:
        match = re.match(r'^crates/([^/]+)/', name)
        if match:
            packages.add(match[1])
        elif name.startswith('src/') or name == 'build.rs':
            packages.add('nestweaver')
    return sorted(packages)


if __name__ == '__main__':
    packages = select(Path(sys.argv[1]).read_text().splitlines())
    Path(sys.argv[2]).write_text(''.join(package + '\n' for package in packages))
    print(f'package_count={len(packages)}')
