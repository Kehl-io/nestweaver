"""Pure fixture safety tests: temporary Git repositories and harmless children only."""
import json
import os
from pathlib import Path
import shutil
import signal
import subprocess
import sys
import tempfile
import time
import unittest
from unittest.mock import Mock, patch

from isolated_daemon import IsolatedDaemon


class FixtureSafetyTests(unittest.TestCase):
    def make_fixture(self):
        fixture = IsolatedDaemon(sys.executable)
        self.addCleanup(shutil.rmtree, fixture.root)
        return fixture

    def test_git_environment_cannot_redirect_fixture_writes(self):
        with tempfile.TemporaryDirectory() as scratch:
            root = Path(scratch)
            external = root / "external"
            external.mkdir()
            clean = {k: v for k, v in os.environ.items() if not k.startswith("GIT_")}
            clean.update(GIT_CONFIG_NOSYSTEM="1", GIT_CONFIG_GLOBAL=os.devnull)
            subprocess.run(["git", "init", str(external)], env=clean,
                           check=True, capture_output=True)
            index = root / "external-index"
            index.write_bytes(b"must remain untouched")
            hooks = root / "hooks"
            hooks.mkdir()
            marker = root / "hook-ran"
            hook = hooks / "pre-commit"
            hook.write_text(f"#!/bin/sh\ntouch '{marker}'\nexit 1\n")
            hook.chmod(0o755)
            config = root / "config"
            config.write_text(f"[core]\n hooksPath = {hooks}\n[commit]\n gpgSign = true\n")
            poison = dict(GIT_DIR=str(external / ".git"), GIT_WORK_TREE=str(external),
                          GIT_INDEX_FILE=str(index), GIT_CONFIG_GLOBAL=str(config),
                          GIT_CONFIG_SYSTEM=str(config), GIT_CONFIG_COUNT="1",
                          GIT_CONFIG_KEY_0="core.hooksPath", GIT_CONFIG_VALUE_0=str(hooks),
                          GIT_CONFIG_PARAMETERS="'commit.gpgSign=true'",
                          GIT_TEMPLATE_DIR=str(hooks), GIT_COMMON_DIR=str(external / ".git"))
            with patch.dict(os.environ, poison):
                fixture = self.make_fixture()
                fixture.create_repository()
            result = subprocess.run(["git", "log", "-1", "--format=%s"], cwd=fixture.repo,
                                    env=fixture.env, capture_output=True, text=True, check=True)
            self.assertEqual(result.stdout.strip(), "fixture")
            self.assertEqual(index.read_bytes(), b"must remain untouched")
            self.assertFalse(marker.exists())
            self.assertFalse(list((external / ".git" / "refs" / "heads").iterdir()))
            self.assertEqual(list(external.iterdir()), [external / ".git"])

    def test_status_accepts_live_fixture(self):
        result = subprocess.CompletedProcess([], 0, json.dumps(dict(
            runtime_telemetry="available", indexing_active=False,
            repo_count=1, degraded_components=[])))
        IsolatedDaemon.assert_daemon_status(result)

    def test_status_rejects_direct_or_empty_response(self):
        for payload in [{}, {"runtime_telemetry": "unavailable"},
                        {"runtime_telemetry": "available", "indexing_active": False,
                         "repo_count": 1, "degraded_components": ["daemon_runtime"]}]:
            with self.subTest(payload=payload), self.assertRaises(AssertionError):
                IsolatedDaemon.assert_daemon_status(
                    subprocess.CompletedProcess([], 0, json.dumps(payload)))

    def signal_case(self, first_signal, group=False):
        with tempfile.TemporaryDirectory() as scratch:
            root = Path(scratch)
            helper = Path(__file__).resolve().parent
            driver = root / "driver.py"
            driver.write_text('''
import os, pathlib, signal, sys, time
sys.path.insert(0, sys.argv[1])
from isolated_daemon import IsolatedDaemon
out = pathlib.Path(sys.argv[2])
class DummyFixture(IsolatedDaemon):
    @staticmethod
    def daemons(): return {}
    def start(self):
        self.log = (self.root / "daemon.log").open("w")
        self.spawn_owned_child([sys.executable, "-c", """
import os, pathlib, signal, sys, time
out = pathlib.Path(sys.argv[1])
def drain(sig, frame):
    (out / 'draining').write_text(str(sig))
    time.sleep(0.4)
    (out / 'drained').write_text('yes')
    sys.exit(0)
def interrupted(sig, frame):
    (out / 'bad-sigint').write_text('unsafe')
    sys.exit(90)
signal.signal(signal.SIGTERM, drain)
signal.signal(signal.SIGINT, interrupted)
(out / 'child-ready').write_text(str(os.getpid()))
while True: time.sleep(0.01)
""", str(out)])
        self.runtime.mkdir(parents=True)
        self.pidfile.write_text(str(self.child.pid))
        (out / "fixture-root").write_text(str(self.root))
        return self
fixture = DummyFixture(sys.executable)
with fixture:
    while not (out / "child-ready").exists(): time.sleep(0.01)
    (out / "ready").write_text(str(os.getpid()))
    while True: time.sleep(0.01)
''')
            process = subprocess.Popen([sys.executable, str(driver), str(helper), str(root)],
                                       start_new_session=True, stdin=subprocess.DEVNULL,
                                       stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
            def wait_file(name):
                deadline = time.monotonic() + 10
                while not (root / name).exists():
                    if process.poll() is not None or time.monotonic() >= deadline:
                        self.fail(f"driver not ready: {name}; exit={process.poll()}")
                    time.sleep(0.01)
            try:
                wait_file("ready")
                child_pid = int((root / "child-ready").read_text())
                self.assertEqual(os.getpgid(child_pid), child_pid)
                self.assertNotEqual(os.getpgid(child_pid), os.getpgid(process.pid))
                if group:
                    os.killpg(process.pid, first_signal)
                else:
                    process.send_signal(first_signal)
                wait_file("draining")
                # Interruption during the bounded drain must not interrupt cleanup.
                process.send_signal(signal.SIGINT)
                process.send_signal(signal.SIGTERM)
                stdout, stderr = process.communicate(timeout=10)
                self.assertEqual(process.returncode, 128 + first_signal, (stdout, stderr))
                self.assertEqual((root / "draining").read_text(), str(int(signal.SIGTERM)))
                self.assertTrue((root / "drained").exists())
                self.assertFalse((root / "bad-sigint").exists())
                with self.assertRaises(ProcessLookupError):
                    os.kill(child_pid, 0)
                fixture_root = Path((root / "fixture-root").read_text())
                evidence = [json.loads(line) for line in
                            (fixture_root / "evidence.jsonl").read_text().splitlines()]
                cleanup = [item for item in evidence if item["kind"] == "cleanup"]
                self.assertEqual(len(cleanup), 1)
                self.assertEqual(cleanup[0]["exit_code"], 0)
            finally:
                if process.poll() is None:
                    process.send_signal(signal.SIGTERM)
                    process.wait(timeout=10)
                if (root / "fixture-root").exists():
                    shutil.rmtree((root / "fixture-root").read_text())

    def test_ctrl_c_group_signal_drains_isolated_child(self):
        self.signal_case(signal.SIGINT, group=True)

    def test_parent_sigterm_drains_child(self):
        self.signal_case(signal.SIGTERM)

    def test_close_is_idempotent(self):
        fixture = self.make_fixture()
        fixture.initial_daemons = {}
        with patch.object(fixture, "daemons", return_value={}):
            fixture.close()
            fixture.close()
        checks = [event for event in fixture.events if event["kind"] == "production_daemon_check"]
        self.assertEqual(len(checks), 1)

    def cleanup_with_child(self, *, already_exited, exit_code):
        fixture = self.make_fixture()
        fixture.initial_daemons = {}
        child = Mock(pid=23456)
        child.returncode = exit_code if already_exited else None
        child.poll.return_value = child.returncode

        def finish_wait(timeout):
            self.assertEqual(timeout, 30)
            child.returncode = exit_code
            return exit_code

        child.wait.side_effect = finish_wait
        fixture.child = child
        return fixture, child

    def test_nonzero_exit_during_graceful_close_is_recorded_and_rejected(self):
        for exit_code in (7, -signal.SIGSEGV):
            with self.subTest(exit_code=exit_code):
                fixture, child = self.cleanup_with_child(already_exited=False,
                                                         exit_code=exit_code)
                with patch.object(fixture, "daemons", return_value={}):
                    with self.assertRaisesRegex(RuntimeError, f"exited with {exit_code}"):
                        fixture.close()
                child.send_signal.assert_called_once_with(signal.SIGTERM)
                child.wait.assert_called_once_with(timeout=30)
                cleanup = [event for event in fixture.events if event["kind"] == "cleanup"]
                self.assertEqual(len(cleanup), 1)
                self.assertEqual(cleanup[0]["exit_code"], exit_code)
                self.assertTrue(any(event["kind"] == "production_daemon_check"
                                    for event in fixture.events))

    def test_already_exited_nonzero_child_cannot_skip_failure_check(self):
        fixture, child = self.cleanup_with_child(already_exited=True, exit_code=9)
        with patch.object(fixture, "daemons", return_value={}):
            with self.assertRaisesRegex(RuntimeError, "exited with 9"):
                fixture.close()
        child.send_signal.assert_not_called()
        child.wait.assert_not_called()
        cleanup = [event for event in fixture.events if event["kind"] == "cleanup"]
        self.assertEqual(len(cleanup), 1)
        self.assertEqual(cleanup[0]["daemon_pid"], child.pid)
        self.assertEqual(cleanup[0]["exit_code"], 9)

    def test_clean_exit_remains_accepted_and_close_remains_idempotent(self):
        for already_exited in (False, True):
            with self.subTest(already_exited=already_exited):
                fixture, child = self.cleanup_with_child(already_exited=already_exited,
                                                         exit_code=0)
                with patch.object(fixture, "daemons", return_value={}):
                    fixture.close()
                    fixture.close()
                if already_exited:
                    child.send_signal.assert_not_called()
                    child.wait.assert_not_called()
                else:
                    child.send_signal.assert_called_once_with(signal.SIGTERM)
                    child.wait.assert_called_once_with(timeout=30)
                cleanup = [event for event in fixture.events if event["kind"] == "cleanup"]
                self.assertEqual(len(cleanup), 1)
                self.assertEqual(cleanup[0]["exit_code"], 0)

    def test_changed_pidfile_does_not_authorize_signaling_another_process(self):
        fixture, child = self.cleanup_with_child(already_exited=False, exit_code=0)
        fixture.runtime.mkdir(parents=True)
        fixture.pidfile.write_text(str(child.pid + 1))
        with self.assertRaisesRegex(RuntimeError, "ownership changed"):
            fixture.close()
        child.send_signal.assert_not_called()
        child.wait.assert_not_called()

    def test_drain_timeout_keeps_child_and_fixture_without_escalation(self):
        fixture, child = self.cleanup_with_child(already_exited=False, exit_code=0)
        child.wait.side_effect = subprocess.TimeoutExpired("owned-child", 30)
        with self.assertRaisesRegex(RuntimeError, "still draining"):
            fixture.close()
        child.send_signal.assert_called_once_with(signal.SIGTERM)
        child.kill.assert_not_called()
        self.assertIsNone(child.returncode)
        self.assertTrue(fixture.root.exists())
        cleanup = [event for event in fixture.events if event["kind"] == "cleanup"]
        self.assertEqual(len(cleanup), 1)
        self.assertIs(cleanup[0]["draining"], True)
        self.assertNotIn("exit_code", cleanup[0])


if __name__ == "__main__":
    unittest.main()
