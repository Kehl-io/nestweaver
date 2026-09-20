#!/usr/bin/env python3
"""Daemon-only release fixture. Never builds binaries or opens a graph store.

Run with --binary /absolute/path/to/nestweaver. Artifacts remain under a unique
/tmp/nw-release-* directory, including on failure. Import IsolatedDaemon for
additional CLI/MCP/HTTP acceptance cases using the same isolated environment.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import signal
import socket
import subprocess
import tempfile
import time


def github_runner_context(environment=None):
    """Same predicate as scripts/ci-direct-cargo.sh. Env values cannot prove origin."""
    environment = os.environ if environment is None else environment
    return environment.get("GITHUB_ACTIONS") == "true" and all(
        environment.get(key) for key in ("RUNNER_TEMP", "RUNNER_OS", "GITHUB_RUN_ID")
    )


class IsolatedDaemon:
    def __init__(self, binary, timeout=120):
        self.binary = Path(binary).resolve(strict=True)
        self.timeout = timeout
        self.root = Path(tempfile.mkdtemp(prefix="nw-release-", dir="/tmp")).resolve()
        self.db = self.root / "fixture.lbug"
        self.repo = self.root / "repo"
        self.env = {k: v for k, v in os.environ.items() if not k.startswith(("NESTWEAVER_", "GIT_"))}
        for name, folder in [("HOME", "home"), ("XDG_CONFIG_HOME", "config"),
                             ("XDG_CACHE_HOME", "cache"), ("XDG_DATA_HOME", "data"),
                             ("XDG_STATE_HOME", "state"), ("XDG_RUNTIME_DIR", "run")]:
            path = self.root / folder
            path.mkdir(mode=0o700)
            self.env[name] = str(path)
        self.env["NESTWEAVER_SOCK_FALLBACK_DIR"] = str(self.root / "sock")
        self.env["NESTWEAVER_DIAGNOSTIC_WIDTH"] = "1000"
        self.env["NESTWEAVER_INDEX_CPU_PERCENT"] = "50"
        # Git must not inherit repository/index/config routing from hooks or
        # shell wrappers. Its system/global configuration is disabled too.
        self.env.update(GIT_CONFIG_NOSYSTEM="1", GIT_CONFIG_SYSTEM=os.devnull,
                        GIT_CONFIG_GLOBAL=os.devnull)
        self.git_empty = self.root / "git-empty"
        self.git_empty.mkdir()
        self.child = None
        self._old_handlers = {}
        self._interrupted = None
        self._spawning = False
        self._closing = False
        self._closed = False
        self.log = None
        self.events = []
        self.instance = hashlib.sha256(os.fsencode(self.db)).hexdigest()[:8]
        self.runtime = self.root / "run" / "nestweaver" / self.instance
        self.pidfile = self.runtime / "daemon.pid"
        self.socket_path = self.runtime / "daemon.sock"
        if len(os.fsencode(self.socket_path)) >= 104:
            self.socket_path = self.root / "sock" / self.instance / "daemon.sock"
        self.initial_daemons = self.daemons()
        self.record(kind="fixture", db=str(self.db), root=str(self.root),
                    binary=str(self.binary), binary_sha256=self.binary_digest(),
                    existing_daemons=self.initial_daemons,
                    source_revision=self.source_revision())

    def source_revision(self):
        result = subprocess.run(["git", "rev-parse", "HEAD"],
                                cwd=Path(__file__).resolve().parents[2], env=self.env,
                                capture_output=True, text=True, timeout=10)
        return result.stdout.strip() if result.returncode == 0 else None

    def binary_digest(self):
        digest = hashlib.sha256()
        with self.binary.open("rb") as source:
            for chunk in iter(lambda: source.read(1024 * 1024), b""):
                digest.update(chunk)
        return digest.hexdigest()

    @staticmethod
    def daemons():
        result = subprocess.run(["ps", "-axo", "pid=,command="], capture_output=True,
                                text=True, check=True, timeout=10)
        return {parts[0]: parts[1] for line in result.stdout.splitlines()
                if len(parts := line.strip().split(maxsplit=1)) == 2
                and "nestweaver" in parts[1] and "daemon" in parts[1]
                and " run" in parts[1]}

    def record(self, **event):
        event["time"] = time.time()
        self.events.append(event)
        with (self.root / "evidence.jsonl").open("a") as output:
            output.write(json.dumps(event) + "\n")

    def run(self, *args, expected=0, timeout=None):
        command = [str(self.binary), *map(str, args)]
        started = time.monotonic()
        try:
            result = subprocess.run(command, cwd=self.root, env=self.env,
                                    capture_output=True, text=True,
                                    timeout=timeout or self.timeout)
        except subprocess.TimeoutExpired as error:
            self.record(kind="cli_timeout", request=command,
                        stdout=str(error.stdout or ""), stderr=str(error.stderr or ""),
                        elapsed_seconds=time.monotonic() - started,
                        daemon_pid=self.child.pid if self.child else None)
            raise
        self.record(kind="cli", request=command, stdout=result.stdout,
                    stderr=result.stderr, exit_code=result.returncode,
                    elapsed_seconds=time.monotonic() - started,
                    daemon_pid=self.child.pid if self.child else None)
        if result.returncode != expected:
            raise RuntimeError(f"{command}: exit {result.returncode}\n{result.stderr}")
        return result

    def _handle_signal(self, signum, _frame):
        if self._interrupted is None:
            self._interrupted = signum
        # Never interrupt child-handle publication or a drain already underway.
        # Further signals cannot turn graceful cleanup into an abrupt exit.
        if not self._spawning and not self._closing:
            raise SystemExit(128 + self._interrupted)

    def install_signal_handlers(self):
        for signum in (signal.SIGINT, signal.SIGTERM):
            self._old_handlers[signum] = signal.signal(signum, self._handle_signal)

    def spawn_owned_child(self, command):
        self._spawning = True
        try:
            self.child = subprocess.Popen(
                command, cwd=self.root, env=self.env, stdin=subprocess.DEVNULL,
                stdout=self.log, stderr=subprocess.STDOUT, start_new_session=True)
        finally:
            self._spawning = False
        if self._interrupted is not None:
            raise SystemExit(128 + self._interrupted)

    def start(self):
        self.run("--version")
        self.log = (self.root / "daemon.log").open("w")
        command = [str(self.binary), "daemon", "--db", str(self.db), "run"]
        self.spawn_owned_child(command)
        self.record(kind="daemon_start", request=command, daemon_pid=self.child.pid)
        deadline = time.monotonic() + self.timeout
        while time.monotonic() < deadline:
            if self.child.poll() is not None:
                raise RuntimeError(f"daemon exited; inspect {self.root / 'daemon.log'}")
            try:
                with socket.socket(socket.AF_UNIX) as probe:
                    probe.settimeout(0.25)
                    probe.connect(str(self.socket_path))
                if int(self.pidfile.read_text().strip()) != self.child.pid:
                    raise RuntimeError("fixture pidfile does not identify the owned child")
                self.run("daemon", "--db", self.db, "status", "--json")
                return self
            except (FileNotFoundError, ConnectionRefusedError, socket.timeout):
                time.sleep(0.1)
        self.record(kind="readiness_timeout", daemon_pid=self.child.pid,
                    socket=str(self.socket_path), timeout_seconds=self.timeout)
        raise TimeoutError(f"daemon readiness timed out; artifacts: {self.root}")

    def create_repository(self):
        self.repo.mkdir()
        (self.repo / "main.js").write_text(
            "export function releaseTarget(name) { return name; }\n"
            "export function releaseCaller() { return releaseTarget('release'); }\n")
        for args in [("init", "--template", str(self.git_empty)), ("add", "."),
                     ("-c", "user.name=Fixture", "-c", "user.email=fixture@example.invalid",
                      "commit", "-m", "fixture")]:
            subprocess.run(["git", "-c", f"core.hooksPath={self.git_empty}",
                            "-c", "commit.gpgSign=false", "-c", "tag.gpgSign=false", *args],
                           cwd=self.repo, env=self.env, stdin=subprocess.DEVNULL,
                           capture_output=True, check=True, timeout=30)

    @staticmethod
    def assert_daemon_status(result):
        payload = json.loads(result.stdout)
        if (payload.get("runtime_telemetry") != "available"
                or not isinstance(payload.get("indexing_active"), bool)
                or payload.get("repo_count") != 1
                or "daemon_runtime" in payload.get("degraded_components", [])):
            raise AssertionError(f"expected indexed fixture and live daemon telemetry: {payload}")

    def bootstrap(self):
        self.create_repository()
        self.run("index", "--repo", self.repo, "--db", self.db)
        self.assert_daemon_status(self.run("brain", "status", "--db", self.db, "--json"))
        result = self.run("impact", "releaseTarget", "--db", self.db, "--json")
        payload = json.loads(result.stdout)
        if payload.get("status") in ("not_found", "ambiguous", "error"):
            raise AssertionError(f"fixture symbol was not resolved: {payload}")
        if "releaseCaller" not in result.stdout:
            raise AssertionError("daemon impact response omitted the fixture caller")
        if int(self.pidfile.read_text().strip()) != self.child.pid:
            raise AssertionError("CLI replaced the fixture daemon")
        self.record(kind="release_daemon_fixture_bootstrap", passed=True,
                    daemon_pid=self.child.pid)

    def check_standard_artifact_policy_in_ci(self):
        if not github_runner_context():
            raise RuntimeError("actual bypass rejection probes are CI-only")
        version = self.run("--version").stdout
        if "+release-fixture-hooks" in version:
            raise AssertionError("fixture-hook artifact must never be distributed")
        original = self.env.copy()
        try:
            for flag in (False, True):
                for request_env in (False, True):
                    for permit in (False, True):
                        if not flag and not request_env:
                            continue
                        self.env = original.copy()
                        if request_env:
                            self.env["NESTWEAVER_NO_DAEMON"] = "1"
                        if permit:
                            self.env["NESTWEAVER_ALLOW_NO_DAEMON"] = "1"
                        args = ["brain", "status", "--db", self.db, "--json"]
                        if flag:
                            args.append("--no-daemon")
                        result = self.run(*args)
                        self.assert_daemon_status(result)
                        if "Routing through the daemon" not in result.stderr:
                            raise AssertionError("standard artifact did not disclose bypass refusal")
                        if int(self.pidfile.read_text().strip()) != self.child.pid:
                            raise AssertionError("rejection probe replaced fixture daemon")
            self.record(kind="release_standard_artifact_cannot_bypass", passed=True)
        finally:
            self.env = original

    def close(self):
        if self._closed or self._closing:
            return
        self._closing = True
        try:
            if self._interrupted is not None:
                self.record(kind="interruption", signal=self._interrupted)
            self._close_owned_child()
        finally:
            self._closed = True
            if self.log:
                self.log.close()
            for signum, previous in self._old_handlers.items():
                signal.signal(signum, previous)
            self._old_handlers.clear()
        if self._interrupted is not None:
            raise SystemExit(128 + self._interrupted)

    def _close_owned_child(self):
        if self.child and self.child.poll() is None:
            # This unreaped child cannot have its PID recycled. We launched it
            # directly with this exact DB, never via launchd/autostart. A changed
            # pidfile refuses cleanup; it never authorizes another signal target.
            if self.pidfile.exists() and int(self.pidfile.read_text().strip()) != self.child.pid:
                raise RuntimeError(f"ownership changed; fixture preserved at {self.root}")
            self.child.send_signal(signal.SIGTERM)
            try:
                self.child.wait(timeout=30)
            except subprocess.TimeoutExpired:
                self.record(kind="cleanup", draining=True, daemon_pid=self.child.pid)
                raise RuntimeError(f"owned daemon still draining; preserved {self.root}")
        if self.child is not None:
            self.record(kind="cleanup", daemon_pid=self.child.pid,
                        exit_code=self.child.returncode)
        if self.log:
            self.log.close()
        after = self.daemons()
        changed = {pid: command for pid, command in self.initial_daemons.items()
                   if after.get(pid) != command}
        self.record(kind="production_daemon_check", unchanged=not changed, changed=changed)
        if changed:
            raise AssertionError(f"pre-existing daemon identity changed: {changed}")
        if self.child is not None and self.child.returncode != 0:
            raise RuntimeError(
                f"owned daemon exited with {self.child.returncode}; preserved {self.root}")

    def __enter__(self):
        self.install_signal_handlers()
        try:
            return self.start()
        except BaseException:
            self.close()
            raise

    def __exit__(self, *_):
        self.close()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", required=True)
    parser.add_argument("--ci-policy-test", action="store_true", help="CI-only standard artifact rejection probes")
    args = parser.parse_args()
    fixture = IsolatedDaemon(args.binary)
    print(f"Release fixture artifacts: {fixture.root}", flush=True)
    with fixture:
        fixture.bootstrap()
        if args.ci_policy_test:
            fixture.check_standard_artifact_policy_in_ci()
    print("release_daemon_fixture_bootstrap: PASS", flush=True)


if __name__ == "__main__":
    main()
