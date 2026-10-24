#!/usr/bin/env python3
"""Standard CLI preserves typed backup refusal across direct and daemon routes."""
import argparse
import hashlib
import os
from pathlib import Path
import shutil

from isolated_daemon import IsolatedDaemon
from backing_file import settle_graph


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--binary', required=True)
    args = parser.parse_args()
    with IsolatedDaemon(args.binary) as fixture:
        fixture.bootstrap()
        settle_graph(fixture)
        other = fixture.root / 'other-database.blob'
        shutil.copyfile(fixture.db, other)
        before = digest(other)
        symlink = fixture.root / 'database-alias'
        symlink.symlink_to(other)
        hardlink = fixture.root / 'database-hardlink'
        os.link(other, hardlink)
        for route in ('daemon', 'direct'):
            if route == 'direct':
                fixture.stop_owned_daemon()
            for destination in (other, symlink, hardlink, fixture.db):
                result = fixture.run('backup', 'save', '--db', fixture.db,
                    destination, '--force', expected=64)
                assert 'refus' in result.stderr.lower() or 'database' in result.stderr.lower(), result
                assert digest(other) == before
                assert symlink.is_symlink()
                fixture.record(kind='typed_backup_refusal', route=route,
                               destination=str(destination), exit_code=result.returncode)
    return 0


if __name__ == '__main__':
    raise SystemExit(main())
