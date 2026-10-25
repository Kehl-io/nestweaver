#!/usr/bin/env python3
"""Standard CLI preserves typed backup refusal across direct and daemon routes."""
import argparse
import hashlib
import json
import time
import os
from pathlib import Path
import shutil

from isolated_daemon import IsolatedDaemon
from backing_file import settle_graph


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def restored_fixture(binary, db):
    fixture = IsolatedDaemon(binary)
    fixture.db = Path(db).resolve()
    fixture.instance = hashlib.sha256(os.fsencode(fixture.db)).hexdigest()[:8]
    fixture.runtime = fixture.root / 'run' / 'nestweaver' / fixture.instance
    fixture.pidfile = fixture.runtime / 'daemon.pid'
    fixture.socket_path = fixture.runtime / 'daemon.sock'
    if len(os.fsencode(fixture.socket_path)) >= 104:
        fixture.socket_path = fixture.root / 'sock' / fixture.instance / 'daemon.sock'
    fixture.record(kind='restored_database_binding', db=str(fixture.db))
    return fixture


def all_bytes(root):
    return {str(path.relative_to(root)): path.read_bytes()
            for path in root.rglob('*') if path.is_file()}


def restored_semantics(binary, db):
    with restored_fixture(binary, db) as fixture:
        deadline = time.monotonic() + 60
        while True:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise AssertionError('Restored derivation did not become Current; raw statuses retained')
            status = json.loads(fixture.run('brain', 'status', '--db', db, '--json',
                                           timeout=min(10, remaining)).stdout)
            fixture.record(kind='restore_derivation_observation', status=status)
            compatibility = status['index_compatibility']
            derivation = status.get('vault_derivation') or {}
            assert compatibility['incomplete_repos'] == [], status
            assert compatibility['resolver_incompatible_repos'] == [], status
            assert not derivation.get('foreign_records') and not derivation.get('blocked_vaults'), status
            if (compatibility['vault_refresh_required'] == []
                    and not derivation.get('in_progress')
                    and derivation.get('pending_or_blocked_vaults', 0) == 0
                    and status['index_publication']['dirty'] is False):
                break
            time.sleep(min(0.25, max(0, deadline-time.monotonic())))
        # Semantic oracles execute ONCE after disclosed readiness, never retry
        # a failed query to hide incomplete publication or ownership errors.
        impact = fixture.run('impact', 'releaseTarget', '--db', db, '--json')
        assert 'releaseCaller' in impact.stdout, impact
        note = fixture.run('note', 'get', 'LiveRestore', '--db', db, '--json')
        assert 'liveRestoreWitness' in note.stdout, note
        fixture.stop_owned_daemon()


def live_repeat_restore(binary):
    with IsolatedDaemon(binary) as source:
        source.bootstrap()
        vault = source.root / 'vault'
        vault.mkdir()
        (vault / 'LiveRestore.md').write_text('# Live restore\n\nliveRestoreWitness calls releaseTarget.\n')
        source.run('brain', 'refresh', vault, '--name', 'live-restore-vault', '--db', source.db)
        settle_graph(source)
        archive = source.root / 'live-source.nwsnap.zst'
        source.run('backup', 'save', archive, '--db', source.db)
        archive_digest = digest(archive)
        source.stop_owned_daemon()
        target = source.root / 'restored'
        source.run('backup', 'restore', archive, '--data-dir', target)
        graph = target / source.db.name
        initial_paths = set(all_bytes(target))
        restored_semantics(binary, graph)
        produced = set(all_bytes(target))
        assert str(Path(source.db.name + '.publications') / 'LOCK') in produced
        assert source.db.name + '.markdown-derivation.json' in produced
        assert source.db.name + '.skipped_notes.json' in produced
        assert any(path.endswith('.store') for path in produced - initial_paths), produced - initial_paths
        previous = set()
        for iteration in range(2):
            before = all_bytes(target)
            response = source.run('backup', 'restore', archive, '--data-dir', target, '--force')
            retained = set(source.root.glob('restored.preserved-*/original'))
            new = retained - previous
            assert len(new) == 1, (retained, previous, response)
            preserved = new.pop()
            assert str(preserved) in response.stdout + response.stderr, response
            assert 'Restore copies were retained for operator review:' in response.stderr, response
            assert 'Complete pre-restore copies' not in response.stderr, response
            assert all_bytes(preserved) == before
            assert digest(archive) == archive_digest
            source.record(kind='live_repeat_restore', iteration=iteration,
                          retained=str(preserved), byte_inventory_preserved=True)
            previous = retained
            restored_semantics(binary, graph)
        # A supported live profile is deliberately narrow: unknown nested
        # contents remain a typed refusal, not an excuse to preserve everything.
        personal = target / (source.db.name + '.publications') / 'unknown' / 'personal.txt'
        personal.parent.mkdir()
        personal.write_bytes(b'personal data must remain untouched')
        before = all_bytes(target)
        source.run('backup', 'restore', archive, '--data-dir', target, '--force', expected=64)
        assert all_bytes(target) == before
        source.record(kind='live_unknown_nested_refusal', preserved=True)
        personal.unlink()
        personal.parent.rmdir()
        # These deliberately invalid wire records test public error mapping,
        # never mint Current/ownership proof or expect a synthetic success.
        aside = target.with_suffix('.restoring')
        journal = Path(str(aside) + '.journal')
        before = all_bytes(target)
        for invalid in ('missing-policy', 'future-policy', 'changed-proof'):
            record = dict(version=4, phase='cleanup', data_dir=str(target),
                          restoring_dir=str(aside), snapshot_path=str(archive))
            if invalid == 'future-policy':
                record['disposition'] = {'policy': 'UnknownFuturePolicy'}
            if invalid == 'changed-proof':
                witnesses = {}
                for path in target.rglob('*'):
                    if path.is_file():
                        meta = path.stat()
                        witnesses[str(path.relative_to(target))] = dict(
                            device=meta.st_dev, inode=meta.st_ino, length=meta.st_size,
                            modified_seconds=meta.st_mtime_ns//1_000_000_000,
                            modified_nanoseconds=meta.st_mtime_ns%1_000_000_000,
                            changed_seconds=meta.st_ctime_ns//1_000_000_000,
                            changed_nanoseconds=meta.st_ctime_ns%1_000_000_000)
                witnesses[source.db.name]['modified_nanoseconds'] += 1
                meta = target.stat()
                record.update(version=3, graph_path=source.db.name,
                    destination_identity=[meta.st_dev, meta.st_ino],
                    destination_paths=[str(path.relative_to(target)) for path in target.rglob('*')],
                    source_checksums={}, destination_file_witnesses=witnesses)
            raw = json.dumps(record).encode()
            journal.write_bytes(raw)
            source.record(kind='deliberately_invalid_recovery_wire', case=invalid, wire=record)
            source.run('backup', 'restore', archive, '--data-dir', target, '--force', expected=64)
            assert all_bytes(target) == before
            assert journal.read_bytes() == raw
            journal.unlink()  # Only this fixture's successful negative control.
        # Public CLI generic operational errors use EXIT_ERROR=1. The native
        # restore_crash_child's exit3 is an internal harness sentinel only.
        missing = source.run('backup', 'restore', source.root/'missing-archive.nwsnap.zst',
                             '--data-dir', target, '--force', expected=1)
        assert 'No such file or directory' in missing.stderr, missing
        assert 'missing-archive.nwsnap.zst' in missing.stderr, missing
        assert all_bytes(target) == before
        source.record(kind='typed_restore_refusal_vs_operational_io', passed=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--binary', required=True)
    parser.add_argument('--live-repeat', action='store_true')
    args = parser.parse_args()
    if args.live_repeat:
        live_repeat_restore(args.binary)
        return 0
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
