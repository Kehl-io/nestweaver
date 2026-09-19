#!/usr/bin/env python3
"""Browser acceptance against an owned daemon and its embedded release UI.

Requires a prebuilt standard binary, installed frontend dependencies, and an
existing browser executable. Never builds or installs. Evidence is retained.
"""
import argparse
import json
from pathlib import Path
import re
import signal
import socket
import subprocess
import time
import urllib.request

from isolated_daemon import IsolatedDaemon


class UiDaemon(IsolatedDaemon):
    def __init__(self, binary, timeout=120):
        super().__init__(binary, timeout=timeout)
        self.ui_child = None
        self.ui_log = None
        self.browser_child = None

    def create_repository(self):
        source = Path(__file__).resolve().parents[2] / "testdata" / "js"
        self.repo.mkdir()
        # Keep greet and the original test fixture for the full existing suite.
        for path in source.rglob("*"):
            if path.is_file() and ".git" not in path.parts:
                target = self.repo / path.relative_to(source)
                target.parent.mkdir(parents=True, exist_ok=True)
                target.write_bytes(path.read_bytes())
        (self.repo / "release-journey.js").write_text(
            "export function releaseA() { return releaseB(); }\n"
            "export function releaseB() { return releaseC(); }\n"
            "export function releaseC() { return 'chain'; }\n"
            "export function releaseD() { return 'unrelated'; }\n"
            "export function releaseTarget(name) { return name; }\n"
            "export function releaseCaller() { return releaseTarget('release'); }\n")
        for args in [("init", "--template", str(self.git_empty)), ("add", "."),
                     ("-c", "user.name=Fixture", "-c", "user.email=fixture@example.invalid",
                      "commit", "-m", "fixture")]:
            subprocess.run(["git", "-c", f"core.hooksPath={self.git_empty}",
                            "-c", "commit.gpgSign=false", "-c", "tag.gpgSign=false", *args],
                           cwd=self.repo, env=self.env, stdin=subprocess.DEVNULL,
                           capture_output=True, check=True, timeout=30)

    def assert_owner(self):
        if self.child.poll() is not None or int(self.pidfile.read_text()) != self.child.pid:
            raise RuntimeError("fixture daemon ownership changed; refusing continuation")

    def spawn_ui_child(self, command):
        self._spawning = True
        try:
            self.ui_child = subprocess.Popen(command, cwd=self.root, env=self.env,
                                            stdin=subprocess.DEVNULL, stdout=self.ui_log,
                                            stderr=subprocess.STDOUT, start_new_session=True)
        finally:
            self._spawning = False
        if self._interrupted is not None:
            raise SystemExit(128 + self._interrupted)

    def start_ui(self, extra_args=()):
        self.assert_owner()
        self.ui_log = (self.root / "ui.log").open("w")
        # This API treats zero as 3000, not as an ephemeral-port request.
        # Choose a free port ourselves; a bind race fails the fixture rather
        # than authorizing attachment to whoever acquired that port.
        with socket.socket() as reservation:
            reservation.bind(("127.0.0.1", 0))
            port = reservation.getsockname()[1]
        command = [str(self.binary), "ui", "--db", str(self.db), "--port", str(port), "--no-open", *extra_args]
        self.spawn_ui_child(command)
        self.record(kind="ui_start", request=command, pid=self.ui_child.pid)
        deadline = time.monotonic() + self.timeout
        while time.monotonic() < deadline:
            self.assert_owner()
            if self.ui_child.poll() is not None:
                raise RuntimeError(f"UI supervisor exited; inspect {self.root / 'ui.log'}")
            match = re.search(r"NestWeaver UI: (http://127\.0\.0\.1:[1-9][0-9]*)",
                              (self.root / "ui.log").read_text())
            if match:
                url = match[1]
                if url != f"http://127.0.0.1:{port}":
                    raise RuntimeError("daemon returned a different UI listener; refusing attachment")
                try:
                    with urllib.request.urlopen(url + "/api/v1/health", timeout=1) as response:
                        health = json.load(response)
                    self.assert_owner()
                    self.record(kind="ui_ready", url=url, health=health,
                                daemon_pid=self.child.pid, supervisor_pid=self.ui_child.pid)
                    return url
                except (OSError, ValueError):
                    pass
            time.sleep(0.1)
        raise TimeoutError(f"UI readiness timed out; artifacts: {self.root}")

    def run_browser(self, frontend, node, browser, specs, grep=None):
        url = self.start_ui()
        env = self.env.copy()
        env.update(NESTWEAVER_UI_FIXTURE_URL=url,
                   NESTWEAVER_UI_RESULTS_DIR=str(self.root / "browser-results"),
                   NESTWEAVER_UI_BROWSER_EXECUTABLE=str(browser))
        command = [str(node), str(frontend / "node_modules/@playwright/test/cli.js"),
                   "test", *specs, "--workers=1", "--retries=0"]
        if grep:
            command.extend(["--grep", grep])
        self.record(kind="browser_start", request=command, fixture_url=url)
        with (self.root / "browser.log").open("w") as output:
            self._spawning = True
            try:
                self.browser_child = subprocess.Popen(command, cwd=frontend, env=env,
                                                       stdout=output, stderr=subprocess.STDOUT,
                                                       stdin=subprocess.DEVNULL,
                                                       start_new_session=True)
            finally:
                self._spawning = False
            if self._interrupted is not None:
                raise SystemExit(128 + self._interrupted)
            code = self.browser_child.wait(timeout=900)
        self.assert_owner()
        self.record(kind="browser_exit", exit_code=code)
        return code

    def _close_owned_child(self):
        # Only unreaped child handles created here can authorize signals.
        # Preserve state if a child cannot drain; never SIGKILL.
        for child, label in [(self.browser_child, "browser"), (self.ui_child, "ui")]:
            if child and child.poll() is None:
                child.send_signal(signal.SIGINT)
                try:
                    child.wait(timeout=30)
                except subprocess.TimeoutExpired:
                    self.record(kind="cleanup", component=label, draining=True, pid=child.pid)
                    raise RuntimeError(f"owned {label} still draining; preserved {self.root}")
                self.record(kind="cleanup", component=label, pid=child.pid,
                            exit_code=child.returncode)
        if self.ui_log:
            self.ui_log.close()
        super()._close_owned_child()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", required=True)
    parser.add_argument("--node", required=True)
    parser.add_argument("--browser", required=True)
    parser.add_argument("--timeout", type=int, default=120)
    parser.add_argument("--grep", help="Run only tests matching this Playwright title regex")
    parser.add_argument("spec", nargs="*", help="Optional specs; default runs the full suite")
    args = parser.parse_args()
    node = Path(args.node).resolve(strict=True)
    browser = Path(args.browser).resolve(strict=True)
    frontend = Path(__file__).resolve().parents[2] / "crates/nestweaver-web/frontend"
    fixture = UiDaemon(args.binary, args.timeout)
    print(f"UI evidence: {fixture.root}", flush=True)
    with fixture:
        fixture.bootstrap()
        code = fixture.run_browser(frontend, node, browser, args.spec, args.grep)
    raise SystemExit(code)


if __name__ == "__main__":
    main()
