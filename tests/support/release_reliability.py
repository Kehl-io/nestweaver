#!/usr/bin/env python3
"""Small, ordinary daemon reliability exercise; never incident-closure evidence.

The 60-second workload includes startup and bootstrap, then allows up to 30
seconds for the owned daemon's normal shutdown. Storage limits are sampled
guards, not filesystem quotas. No builds or production database access.
"""
import argparse
from collections import Counter
import json
import os
from pathlib import Path
import shutil
import subprocess
import threading
import time

from isolated_daemon import IsolatedDaemon

MIB = 1024 * 1024
SOURCE_LIMIT = 32 * MIB
STORAGE_LIMIT = 256 * MIB
STORAGE_GUARD = 192 * MIB
FREE_FLOOR = 20 * 1024 * MIB


class Incomplete(RuntimeError):
    pass


class ReliabilityDaemon(IsolatedDaemon):
    def __init__(self, binary, seconds):
        self.lock = threading.RLock()
        self.stop = threading.Event()
        self.deadline = time.monotonic() + seconds
        self.counts = Counter()
        self.errors = []
        self.intervals = []
        self.high_water = 0
        self.minimum_free = shutil.disk_usage('/tmp').free
        self.indexing_observed = False
        super().__init__(binary, timeout=min(10, seconds))
        self.env.pop('CI', None)
        self.env.pop('GITHUB_ACTIONS', None)

    def record(self, **event):
        with self.lock:
            super().record(**event)

    def _handle_signal(self, signum, _frame):
        # Let the bounded request loops unwind. Repeated signals must not
        # interrupt worker joins or prevent the owned daemon's graceful drain.
        if self._interrupted is None:
            self._interrupted = signum
        self.stop.set()

    def sample(self):
        size = 0
        for base, _, files in os.walk(self.root):
            for filename in files:
                try:
                    stat = (Path(base) / filename).lstat()
                    size += max(stat.st_size, stat.st_blocks * 512)
                except FileNotFoundError:
                    pass
        free = shutil.disk_usage(self.root).free
        with self.lock:
            self.high_water = max(size, self.high_water)
            self.minimum_free = min(free, self.minimum_free)
        if size >= STORAGE_GUARD or free < FREE_FLOOR:
            raise Incomplete(f'resource guard: fixture={size}, free={free}')
        if self.child and not self._closing:
            if self.child.poll() is not None:
                raise RuntimeError(f'owned daemon exited: {self.child.returncode}')
            if self.pidfile.exists() and int(self.pidfile.read_text()) != self.child.pid:
                raise RuntimeError('daemon ownership changed')

    def execute(self, command, kind, cwd=None, timeout=10):
        self.sample()
        if self.stop.is_set() or time.monotonic() >= self.deadline:
            raise Incomplete('workload deadline or stop reached before request')
        started = time.monotonic()
        end = min(self.deadline, started + timeout)
        # Files keep subprocess output out of unbounded in-memory pipes. The
        # watchdog includes these files in its storage sample every 100 ms.
        with self.lock:
            sequence = self.counts['started']
            self.counts['started'] += 1
        out_path = self.root / f'command-{sequence:04d}.stdout'
        err_path = self.root / f'command-{sequence:04d}.stderr'
        process = None
        error = None
        with out_path.open('wb') as output, err_path.open('wb') as stderr:
            try:
                process = subprocess.Popen(command, cwd=cwd or self.root,
                                           env=self.env, stdin=subprocess.DEVNULL,
                                           stdout=output, stderr=stderr)
                while process.poll() is None:
                    if self.stop.wait(0.1):
                        raise Incomplete('request cancelled after stop')
                    self.sample()
                    if time.monotonic() >= end:
                        raise Incomplete(f'{kind} deadline reached')
            except BaseException as exc:
                error = exc
            finally:
                if process and process.poll() is None:
                    # Cancel only this directly owned CLI, never another PID.
                    process.terminate()
                    try:
                        process.wait(timeout=2)
                    except subprocess.TimeoutExpired:
                        process.kill()
                        process.wait(timeout=2)
        finished = time.monotonic()
        with out_path.open('rb') as output:
            stdout = output.read(64 * 1024).decode('utf-8', 'replace')
        with err_path.open('rb') as output:
            stderr_text = output.read(64 * 1024).decode('utf-8', 'replace')
        result = subprocess.CompletedProcess(command, process.returncode if process else None,
                                             stdout, stderr_text)
        self.record(kind='reliability_command', operation=kind, request=command,
                    cli_pid=process.pid if process else None,
                    daemon_pid=self.child.pid if self.child else None,
                    started_monotonic=started, finished_monotonic=finished,
                    elapsed_seconds=finished - started, exit_code=result.returncode,
                    stdout_file=str(out_path), stderr_file=str(err_path),
                    error=repr(error) if error else None)
        with self.lock:
            self.intervals.append((kind, started, finished, result.returncode))
        if error:
            raise error
        if result.returncode != 0:
            raise RuntimeError(f'{kind}: exit {result.returncode}: {stderr_text[:2000]}')
        with self.lock:
            self.counts[kind] += 1
        return result

    def run(self, *args, expected=0, timeout=None):
        if expected != 0:
            raise ValueError('this exercise only accepts successful ordinary commands')
        return self.execute([str(self.binary), *map(str, args)], 'setup',
                            timeout=timeout or 10)

    def prepare_repository(self):
        self.repo.mkdir()
        # Stable names and ordinary function calls; less than 64 KiB total.
        for number in range(64):
            text = (f'export function sample{number}(value) {{ return value + {number}; }}\n'
                    f'export function useSample{number}() {{ return sample{number}(1); }}\n')
            (self.repo / f'module{number:03d}.js').write_text(text)
        (self.repo / 'main.js').write_text(
            "export function releaseTarget(value) { return value; }\n"
            "export function releaseCaller() { return releaseTarget('ordinary'); }\n")
        source_bytes = sum(p.stat().st_size for p in self.repo.glob('*.js'))
        if source_bytes > SOURCE_LIMIT:
            raise Incomplete('generated source exceeds fixed source limit')
        for args in [('init', '--template', str(self.git_empty)), ('add', '.'),
                     ('-c', 'user.name=Fixture', '-c', 'user.email=fixture@example.invalid',
                      'commit', '-m', 'ordinary reliability fixture')]:
            self.execute(['git', '-c', f'core.hooksPath={self.git_empty}',
                          '-c', 'commit.gpgSign=false', *args], 'git', cwd=self.repo)
        self.record(kind='corpus', files=65, source_bytes=source_bytes,
                    source_byte_limit=SOURCE_LIMIT, content='valid JavaScript functions')

    def query(self, kind):
        args = {'impact': ['impact', 'releaseTarget', '--limit', '10'],
                'search': ['search', 'releaseTarget', '--limit', '10'],
                'status': ['brain', 'status']}[kind]
        result = self.execute([str(self.binary), *args, '--db', str(self.db), '--json'], kind)
        payload = json.loads(result.stdout)
        if kind == 'status':
            self.assert_daemon_status(result)
            self.indexing_observed |= payload['indexing_active']
        elif kind == 'impact':
            local = payload.get('local_impact', payload)
            if local.get('status') != 'ok' or 'releaseCaller' not in result.stdout:
                raise AssertionError('impact did not resolve target and its caller')
        elif 'releaseTarget' not in result.stdout:
            raise AssertionError('search did not return known fixture target')

    def index(self):
        self.execute([str(self.binary), 'index', '--repo', str(self.repo),
                      '--db', str(self.db)], 'index', timeout=30)

    def worker(self, offset):
        try:
            kinds = ('impact', 'search', 'status')
            sequence = offset
            while not self.stop.is_set() and time.monotonic() < self.deadline - 10:
                self.query(kinds[sequence % len(kinds)])
                sequence += 1
                self.stop.wait(0.5)
        except BaseException as error:
            self.fail(error)

    def fail(self, error):
        with self.lock:
            self.errors.append({'type': type(error).__name__, 'message': str(error)})
        self.stop.set()

    def monitor(self, done):
        try:
            while not done.wait(0.1):
                self.sample()
        except BaseException as error:
            self.fail(error)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--binary', required=True)
    parser.add_argument('--seconds', type=int, default=60, choices=range(20, 61), metavar='20..60')
    args = parser.parse_args()
    if shutil.disk_usage('/tmp').free < FREE_FLOOR:
        parser.error('requires at least 20 GiB free on the fixture filesystem')
    fixture = ReliabilityDaemon(args.binary, args.seconds)
    print(f'Reliability artifacts: {fixture.root}', flush=True)
    threads = []
    monitor_done = threading.Event()
    monitor = threading.Thread(target=fixture.monitor, args=(monitor_done,))
    monitor.start()
    clean_shutdown = False
    fixture.install_signal_handlers()
    try:
        fixture.start()
        fixture.prepare_repository()
        fixture.index()
        for kind in ('status', 'impact', 'search'):
            fixture.query(kind)
        threads = [threading.Thread(target=fixture.worker, args=(n,)) for n in range(2)]
        for thread in threads:
            thread.start()
        revision = 0
        while not fixture.stop.is_set() and time.monotonic() < fixture.deadline - 10:
            revision += 1
            # Mutate one regular source file only after the preceding index
            # completed; keep names/caller topology stable for concurrent reads.
            (fixture.repo / 'module000.js').write_text(
                f'export function sample0(value) {{ return value + {revision}; }}\n'
                'export function useSample0() { return sample0(1); }\n')
            fixture.index()
            fixture.stop.wait(1)
        for thread in threads:
            thread.join(max(0, fixture.deadline - time.monotonic()) + 3)
        if any(thread.is_alive() for thread in threads):
            raise Incomplete('query workers did not drain by workload deadline')
        fixture.sample()
        if not fixture.errors:
            fixture.query('status')
    except BaseException as error:
        fixture.fail(error)
    finally:
        fixture.stop.set()
        for thread in threads:
            thread.join(5)
        try:
            if any(thread.is_alive() for thread in threads):
                raise Incomplete('query workers still draining; daemon preserved without signalling')
            fixture.close()
            clean_shutdown = bool(fixture.child and fixture.child.returncode == 0)
        except BaseException as error:
            fixture.fail(error)
        finally:
            monitor_done.set()
            monitor.join(5)
    successful = [(kind, start, end) for kind, start, end, code in fixture.intervals if code == 0]
    indexes = [(start, end) for kind, start, end in successful if kind == 'index']
    overlapping_queries = sum(any(start < index_end and end > index_start
                                  for index_start, index_end in indexes)
                              for kind, start, end in successful if kind in ('impact', 'search', 'status'))
    complete = (not fixture.errors and clean_shutdown and len(indexes) >= 2
                and all(fixture.counts[kind] >= 2 for kind in ('impact', 'search', 'status'))
                and overlapping_queries > 0)
    outcome = 'PASS' if complete else ('FAIL' if any(e['type'] != 'Incomplete' for e in fixture.errors)
                                         or not clean_shutdown else 'INCOMPLETE')
    summary = dict(outcome=outcome, completed_cli_counts=dict(fixture.counts),
                   errors=fixture.errors, clean_shutdown=clean_shutdown,
                   command_interval_overlap_count=overlapping_queries,
                   indexing_active_observed=fixture.indexing_observed,
                   fixture_high_water_bytes=fixture.high_water,
                   minimum_free_bytes=fixture.minimum_free,
                   storage_guard_bytes=STORAGE_GUARD, storage_limit_bytes=STORAGE_LIMIT,
                   query_workers=2, workload_seconds=args.seconds,
                   limitation='Ordinary reliability evidence only; CLI interval overlap does not prove concurrent database execution. No checkpoint measurement or incident closure. Storage is sampled, not a quota.')
    fixture.record(kind='reliability_summary', **summary)
    (fixture.root / 'summary.json').write_text(json.dumps(summary, indent=2) + '\n')
    print(json.dumps(summary, indent=2), flush=True)
    return 0 if complete else 1


if __name__ == '__main__':
    raise SystemExit(main())
