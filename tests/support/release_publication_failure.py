#!/usr/bin/env python3
"""Controlled publication-failure lifecycle through owned fixture daemons.

Requires a prebuilt release-fixture-hooks binary. Never builds or opens a store.
Cases: committed-content error, post-SHA generation save, client interruption,
and autonomous manifest-save recovery. Each case uses a fresh owned daemon.
"""
import argparse
import json
import os
from pathlib import Path
import secrets
import shutil
import signal
import stat
import subprocess
import time

from isolated_daemon import IsolatedDaemon


class PublicationFailureDaemon(IsolatedDaemon):
    def __init__(self, binary):
        super().__init__(binary, timeout=120)
        self.deadline = time.monotonic() + 120
        self.control = self.root / "fixture-control"
        self.control.mkdir(mode=0o700)
        self.nonce = secrets.token_hex(32)
        self.index_child = None
        self.index_attempt = 0
        self.atomic_record("activation.json", {
            "protocol": 1, "nonce": self.nonce, "owner_pid": os.getpid(),
            "database": str(self.db),
        })

    def atomic_record(self, name, value):
        pending = self.control / (name + ".pending")
        with pending.open("x") as output:
            os.chmod(pending, 0o600)
            json.dump(value, output)
            output.flush()
        pending.replace(self.control / name)

    def check_bounds(self):
        if time.monotonic() >= self.deadline:
            raise TimeoutError("fixture incomplete: 120-second admission deadline")
        if shutil.disk_usage(self.root).free < 20 * 1024**3:
            raise RuntimeError("fixture incomplete: free space below 20 GiB")
        size = 0
        for path in self.root.rglob("*"):
            try:
                info = path.lstat()
            except FileNotFoundError:
                # Ordinary atomic sidecar publication removes temporary paths.
                continue
            if stat.S_ISREG(info.st_mode):
                size += info.st_size
        if size > 256 * 1024**2:
            raise RuntimeError("fixture incomplete: storage exceeded 256 MiB")
        if self.child is not None:
            if self.child.poll() is not None or int(self.pidfile.read_text()) != self.child.pid:
                raise RuntimeError("fixture incomplete: daemon ownership changed")

    def spawn_owned_child(self, command):
        # Mutate the same request list that the base helper records afterwards.
        command.extend(["--release-fixture-control", str(self.control)])
        super().spawn_owned_child(command)

    def start(self):
        self.check_bounds()
        version = self.run("--version").stdout
        if "+release-fixture-hooks" not in version:
            raise RuntimeError("fixture binary lacks explicit feature provenance")
        super().start()
        self.check_bounds()
        events = self.receipts()
        if [event["kind"] for event in events] != ["activation_checked", "identity_bound"]:
            raise RuntimeError("fixture incomplete: missing exact activation receipts")
        self.brain_uuid = events[-1]["brain_uuid"]
        if not isinstance(self.brain_uuid, str) or not self.brain_uuid:
            raise RuntimeError("fixture persistent identity missing")
        return self

    def receipts(self):
        raw = (self.control / "receipts.jsonl").read_bytes()
        if len(raw) > 64 * 1024:
            raise RuntimeError("fixture receipts exceed bound")
        # A live writer may have started its next line; only complete records
        # count as evidence, and final acceptance requires the final receipt.
        lines = raw.split(b"\n")[:-1]
        events = [json.loads(line) for line in lines]
        for event in events:
            if (event["protocol"] != 1 or event["nonce"] != self.nonce
                    or event["daemon_pid"] != self.child.pid
                    or event["database"] != str(self.db)):
                raise RuntimeError("fixture receipt identity mismatch")
            if hasattr(self, "brain_uuid") and event["kind"] != "activation_checked":
                if event["brain_uuid"] != self.brain_uuid:
                    raise RuntimeError("fixture receipt data identity changed")
        return events

    def run_index(self, *, force=False, interrupt=False):
        self.check_bounds()
        self.index_attempt += 1
        if self.index_attempt > 2:
            raise RuntimeError("fixture index-attempt bound exceeded")
        command = [str(self.binary), "index", "--repo", str(self.repo), "--db", str(self.db)]
        if force:
            command.append("--force")
        stdout_path = self.root / f"index-{self.index_attempt}.stdout"
        stderr_path = self.root / f"index-{self.index_attempt}.stderr"
        started = time.monotonic()
        interrupted = False
        with stdout_path.open("w") as stdout, stderr_path.open("w") as stderr:
            self._spawning = True
            try:
                self.index_child = subprocess.Popen(
                    command, cwd=self.root, env=self.env, stdin=subprocess.DEVNULL,
                    stdout=stdout, stderr=stderr, start_new_session=True)
            finally:
                self._spawning = False
            if self._interrupted is not None:
                raise SystemExit(128 + self._interrupted)
            while self.index_child.poll() is None:
                self.check_bounds()
                if interrupt and not interrupted:
                    self.index_child.send_signal(signal.SIGINT)
                    interrupted = True
                    self.record(kind="fixture_index_interrupted", pid=self.index_child.pid,
                                daemon_pid=self.child.pid)
                time.sleep(0.1)
            code = self.index_child.returncode
        self.record(kind="fixture_index", request=command, exit_code=code,
                    stdout=stdout_path.read_text(), stderr=stderr_path.read_text(),
                    elapsed_seconds=time.monotonic() - started, daemon_pid=self.child.pid,
                    interrupted=interrupted)
        self.check_bounds()
        return code, stdout_path.read_text() + stderr_path.read_text()

    def idle_status(self):
        while True:
            self.check_bounds()
            status = json.loads(self.run("brain", "status", "--db", self.db,
                                         "--json", timeout=10).stdout)
            if (status.get("runtime_telemetry") == "available"
                    and status.get("indexing_active") is False
                    and status.get("write_holder") == ""
                    and status.get("write_queue_depth") == 0):
                return status
            time.sleep(0.1)

    def _close_owned_child(self):
        # Any CLI still alive is first interrupted and drained; never killed,
        # and the fixture stays intact.
        if self.index_child is not None and self.index_child.poll() is None:
            self.index_child.send_signal(signal.SIGINT)
            try:
                self.index_child.wait(timeout=30)
            except subprocess.TimeoutExpired as error:
                self.record(kind="cleanup", component="index", draining=True,
                            pid=self.index_child.pid)
                raise RuntimeError(f"owned index still draining; preserved {self.root}") from error
            self.record(kind="cleanup", component="index", pid=self.index_child.pid,
                        exit_code=self.index_child.returncode)
        super()._close_owned_child()


def prepare_repo(fixture):
    fixture.create_repository()
    files = [path for path in fixture.repo.rglob("*")
             if path.is_file() and ".git" not in path.parts]
    assert len(files) <= 8 and sum(path.stat().st_size for path in files) <= 1024**2
    head = subprocess.run(["git", "rev-parse", "HEAD"], cwd=fixture.repo,
                          env=fixture.env, capture_output=True, text=True,
                          check=True, timeout=10).stdout.strip()
    return head


def arm_command(fixture, stage, head):
    request_id = secrets.token_hex(16)
    fixture.atomic_record("command.json", {
        "protocol": 1, "nonce": fixture.nonce, "brain_uuid": fixture.brain_uuid,
        "daemon_pid": fixture.child.pid, "sequence": 1, "request_id": request_id,
        "stage": stage, "action": "return_error",
        "repo_root": str(fixture.repo.resolve()), "target_sha": head,
        "expires_unix_ms": int(time.time() * 1000) + 30_000,
    })
    return request_id


def list_repos(fixture):
    return json.loads(fixture.run("list-repos", "--db", fixture.db, "--json", timeout=10).stdout)


def impact_ok(fixture):
    result = json.loads(fixture.run("impact", "releaseTarget", "--db", fixture.db,
                                    "--json", timeout=10).stdout)
    assert result.get("status") == "ok" and "releaseCaller" in json.dumps(result), result
    return result


def wait_receipt_kinds(fixture, kinds):
    deadline = time.monotonic() + 30
    while time.monotonic() < deadline:
        fixture.check_bounds()
        receipts = fixture.receipts()
        found = [event["kind"] for event in receipts]
        if all(kind in found for kind in kinds):
            return receipts
        time.sleep(0.1)
    raise AssertionError(f"missing receipt kinds {kinds}: {fixture.receipts()}")


def wait_suggestions(fixture):
    deadline = time.monotonic() + 40
    while time.monotonic() < deadline:
        fixture.check_bounds()
        result = subprocess.run(
            [str(fixture.binary), "suggest-links", "--db", str(fixture.db), "--json"],
            cwd=fixture.root, env=fixture.env, capture_output=True, text=True, timeout=10)
        fixture.record(kind="suggest_links", exit_code=result.returncode,
                       stdout=result.stdout, stderr=result.stderr,
                       daemon_pid=fixture.child.pid)
        if result.returncode == 0:
            payload = json.loads(result.stdout)
            if isinstance(payload, dict) and "error" not in payload:
                return payload
        time.sleep(0.25)
    raise AssertionError("suggestions did not recover after the injected manifest failure")


def case_one(fixture):
    head = prepare_repo(fixture)
    request_id = arm_command(fixture, "index_content_committed", head)
    code, output = fixture.run_index()
    assert code != 0 and "fixture_content_commit_error" in output, (code, output)
    after_error = fixture.idle_status()
    receipts = fixture.receipts()
    expected = ["activation_checked", "identity_bound", "armed", "reached",
                "error_injected", "engine_completed", "operation_scope_finished"]
    assert [event["kind"] for event in receipts] == expected, receipts
    operation = receipts[2:]
    assert all(event["details"]["request_id"] == request_id for event in operation)
    assert all(event["details"]["sequence"] == 1 for event in operation)
    uids = {event["details"]["repo_uid"] for event in operation}
    assert len(uids) == 1
    uid = uids.pop()
    reached = receipts[3]["details"]["evidence"]
    assert reached["committed_files"] > 0 and reached["committed_symbols"] > 0
    assert reached["indexed_sha"] == "" and reached["target_sha"] == head
    completed = receipts[5]["details"]["evidence"]
    assert completed["reached"] and "fixture_content_commit_error" in completed["error"]
    assert completed["indexed_sha"] == ""
    repos = list_repos(fixture)
    assert len(repos) == 1 and repos[0]["uid"] == uid and repos[0]["indexed_sha"] == "", repos
    fixture.record(kind="committed_content_error_observed", receipts=receipts,
                   target_head=head, repo_uid=uid, status=after_error)

    # The same failed graph can have a clean publication after the normal
    # finalizer. A clean marker alone never turns the failed job into success.
    code, output = fixture.run_index(force=True)
    assert code == 0, ("forced retry did not complete", code, output)
    final_status = fixture.idle_status()
    repos = list_repos(fixture)
    assert len(repos) == 1 and repos[0]["uid"] == uid and repos[0]["indexed_sha"] == head, repos
    assert final_status["index_publication"]["dirty"] is False, final_status
    impact_ok(fixture)
    assert fixture.receipts() == receipts, "one-shot hook was unexpectedly reused"
    fixture.check_bounds()
    fixture.record(kind="release_content_commit_failure_retry_observed",
                   target_head=head, repo_uid=uid, status=final_status)


def case_two(fixture):
    head = prepare_repo(fixture)
    request_id = arm_command(fixture, "index_generation_save", head)
    code, output = fixture.run_index()
    assert code != 0 and "fixture_generation_save_error" in output, (code, output)
    after_error = fixture.idle_status()
    receipts = fixture.receipts()
    expected = ["activation_checked", "identity_bound", "armed", "reached",
                "error_injected", "engine_completed", "operation_scope_finished"]
    assert [event["kind"] for event in receipts] == expected, receipts
    operation = receipts[2:]
    assert all(event["details"]["request_id"] == request_id for event in operation)
    uids = {event["details"]["repo_uid"] for event in operation}
    assert len(uids) == 1
    uid = uids.pop()
    reached = receipts[3]["details"]["evidence"]
    assert reached["indexed_sha"] == head and reached["target_sha"] == head
    completed = receipts[5]["details"]["evidence"]
    assert completed["reached"] and "fixture_generation_save_error" in completed["error"]
    assert completed["indexed_sha"] == head
    repos = list_repos(fixture)
    assert len(repos) == 1 and repos[0]["uid"] == uid and repos[0]["indexed_sha"] == head, repos
    assert after_error["index_publication"]["dirty"] is True, after_error
    fixture.record(kind="generation_save_error_observed", receipts=receipts,
                   target_head=head, repo_uid=uid, status=after_error)

    code, output = fixture.run_index(force=True)
    assert code == 0, ("forced retry did not complete", code, output)
    final_status = fixture.idle_status()
    repos = list_repos(fixture)
    assert len(repos) == 1 and repos[0]["uid"] == uid and repos[0]["indexed_sha"] == head, repos
    assert final_status["index_publication"]["dirty"] is False, final_status
    impact_ok(fixture)
    assert fixture.receipts() == receipts, "one-shot hook was unexpectedly reused"
    fixture.check_bounds()
    fixture.record(kind="release_generation_save_failure_retry_observed",
                   target_head=head, repo_uid=uid, status=final_status)


def case_three(fixture):
    head = prepare_repo(fixture)
    code, _output = fixture.run_index(interrupt=True)
    after_interrupt = fixture.idle_status()
    fixture.record(kind="client_interruption_observed", exit_code=code,
                   target_head=head, status=after_interrupt)
    code, output = fixture.run_index(force=True)
    assert code == 0, ("retry after client interruption did not complete", code, output)
    final_status = fixture.idle_status()
    repos = list_repos(fixture)
    assert len(repos) == 1 and repos[0]["indexed_sha"] == head, repos
    assert final_status["index_publication"]["dirty"] is False, final_status
    impact_ok(fixture)
    fixture.check_bounds()
    fixture.record(kind="release_client_interruption_retry_observed",
                   target_head=head, repo_uid=repos[0]["uid"], status=final_status)


def case_four(fixture):
    head = prepare_repo(fixture)
    code, output = fixture.run_index()
    assert code == 0, ("baseline index before manifest hook failed", code, output)
    baseline = fixture.idle_status()
    repos = list_repos(fixture)
    assert len(repos) == 1 and repos[0]["indexed_sha"] == head, repos
    request_id = arm_command(fixture, "manifest_before_save", head)
    (fixture.repo / "extra.js").write_text("export function releaseExtra() { return 1; }\n")
    subprocess.run(
        ["git", "-c", f"core.hooksPath={fixture.git_empty}",
         "-c", "commit.gpgSign=false", "-c", "tag.gpgSign=false",
         "-c", "user.name=Fixture", "-c", "user.email=fixture@example.invalid",
         "add", "extra.js"],
        cwd=fixture.repo, env=fixture.env, capture_output=True, check=True, timeout=10)
    subprocess.run(
        ["git", "-c", f"core.hooksPath={fixture.git_empty}",
         "-c", "commit.gpgSign=false", "-c", "tag.gpgSign=false",
         "-c", "user.name=Fixture", "-c", "user.email=fixture@example.invalid",
         "commit", "-m", "manifest-hook"],
        cwd=fixture.repo, env=fixture.env, capture_output=True, check=True, timeout=10)
    code, output = fixture.run_index(force=True)
    assert code == 0, ("forced index under manifest hook failed", code, output)
    receipts = wait_receipt_kinds(fixture, ["reached", "error_injected"])
    injected = [event for event in receipts if event["kind"] == "error_injected"]
    assert injected and injected[0]["details"]["error_code"] == "fixture_manifest_save_error", receipts
    reached = [event for event in receipts if event["kind"] == "reached"]
    assert reached and reached[0]["details"]["request_id"] == request_id, receipts
    payload = wait_suggestions(fixture)
    final_status = fixture.idle_status()
    assert final_status["index_publication"]["dirty"] is False, final_status
    impact_ok(fixture)
    fixture.check_bounds()
    fixture.record(kind="release_manifest_save_failure_autonomous_recovery_observed",
                   target_head=head, repo_uid=repos[0]["uid"], status=final_status,
                   baseline=baseline, suggestions=payload, receipts=receipts)


CASES = [
    ("1", "release_content_commit_failure_retry", case_one),
    ("2", "release_generation_save_failure_retry", case_two),
    ("3", "release_client_interruption_retry", case_three),
    ("4", "release_manifest_save_failure_autonomous_recovery", case_four),
]


def run_case(binary, case_fn, label):
    fixture = PublicationFailureDaemon(binary)
    print(f"{label} evidence: {fixture.root}", flush=True)
    try:
        with fixture:
            case_fn(fixture)
    except BaseException as error:
        fixture.record(kind="acceptance_incomplete", error=f"{type(error).__name__}: {error}")
        raise
    fixture.record(kind=label, passed=True, daemon_exit_code=fixture.child.returncode)
    print(f"{label}: PASS", flush=True)
    return fixture


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", required=True)
    parser.add_argument("--case", choices=[key for key, _, _ in CASES] + ["all"], default="all")
    args = parser.parse_args()
    selected = CASES if args.case == "all" else [item for item in CASES if item[0] == args.case]
    for _key, label, case_fn in selected:
        run_case(args.binary, case_fn, label)


if __name__ == "__main__":
    main()
