#!/usr/bin/env python3
"""Real-daemon backing-file admission regressions; never builds or opens a store."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import time

from isolated_daemon import IsolatedDaemon


def digest(path):
    result = hashlib.sha256()
    with path.open("rb") as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            result.update(chunk)
    return result.hexdigest()


def settle_graph(fixture):
    """Observe empty daemon work queues and a stable file before invalidation."""
    deadline = time.monotonic() + 30
    previous = None
    stable = 0
    while time.monotonic() < deadline:
        status = json.loads(fixture.run("brain", "status", "--db", fixture.db, "--json").stdout)
        embedding = status.get("embedding_status") or {}
        idle = (status.get("indexing_active") is False
                and status.get("queue_depth") == 0
                and status.get("write_queue_depth") == 0
                and status.get("write_holder") == ""
                and embedding.get("pass_active") is False
                and embedding.get("seed_active") is False)
        current = digest(fixture.db) if idle else None
        stable = stable + 1 if current is not None and current == previous else 0
        previous = current
        if stable >= 3:
            fixture.record(kind="backing_file_quiescence", idle_status=status,
                           stable_digest=current, stable_observations=stable + 1)
            return current
        time.sleep(0.15)
    raise AssertionError("fixture graph did not reach empty queues and stable backing bytes")


def assert_incumbent(fixture, original_pid):
    if fixture.child.poll() is not None:
        raise AssertionError("the unhealthy daemon exited before graceful shutdown")
    if int(fixture.pidfile.read_text().strip()) != original_pid:
        raise AssertionError("a request replaced the unhealthy incumbent daemon")
    current = fixture.daemons()
    if str(original_pid) not in current:
        raise AssertionError("the owned daemon disappeared from the process inventory")
    for pid, command in current.items():
        if pid != str(original_pid) and str(fixture.db) in command:
            raise AssertionError(f"a second writer was started: {pid}: {command}")


def refused_write(fixture, *arguments):
    command = [str(fixture.binary), *map(str, arguments)]
    started = time.monotonic()
    result = subprocess.run(command, cwd=fixture.root, env=fixture.env,
                            stdin=subprocess.DEVNULL, capture_output=True,
                            text=True, timeout=60)
    fixture.record(kind="backing_file_write_refusal", request=command,
                   exit_code=result.returncode, stdout=result.stdout,
                   stderr=result.stderr, elapsed_seconds=time.monotonic() - started,
                   daemon_pid=fixture.child.pid)
    if result.returncode <= 0:
        raise AssertionError(f"write was not refused: {command}: {result}")
    evidence = (result.stdout + result.stderr).lower()
    if not any(word in evidence for word in ("backing", "orphaned", "was deleted", "was replaced")):
        raise AssertionError(f"write failed without backing-file refusal evidence: {result}")


def exercise(binary, mutation):
    fixture = IsolatedDaemon(binary)
    print(f"Backing-file {mutation} artifacts: {fixture.root}", flush=True)
    with fixture:
        fixture.bootstrap()
        original_pid = fixture.child.pid
        before = settle_graph(fixture)
        # Keep the exact inode reachable so cleanup can restore the owned path.
        # Each case has a fresh daemon and DB; no production files are touched.
        preserved = fixture.root / "original-backing-file"
        os.link(fixture.db, preserved)
        if digest(preserved) != before:
            raise AssertionError("graph changed after the quiescence witness")
        replacement_digest = None
        moved = fixture.root / "renamed-backing-file"
        try:
            if mutation == "unlink":
                fixture.db.unlink()
            elif mutation == "rename":
                fixture.db.rename(moved)
            elif mutation == "replacement":
                replacement = fixture.root / "replacement-backing-file"
                shutil.copyfile(preserved, replacement)
                replacement_digest = digest(replacement)
                os.replace(replacement, fixture.db)
            else:
                raise AssertionError(f"unknown mutation: {mutation}")
            fixture.record(kind="backing_file_mutation", mutation=mutation,
                           original_pid=original_pid,
                           original_inode=preserved.stat().st_ino,
                           current_inode=fixture.db.stat().st_ino if fixture.db.exists() else None)
            status = fixture.run("daemon", "--db", fixture.db, "status", "--json", expected=69)
            health = json.loads(status.stdout)
            if (health.get("state") != "unhealthy"
                    or health.get("backing_file_valid") is not False
                    or health.get("evidence") != "backing_file_identity"
                    or health.get("pid") != original_pid
                    or not health.get("backing_file_error")):
                raise AssertionError(f"expected live unhealthy backing-file status: {health}")
            assert_incumbent(fixture, original_pid)
            with (fixture.repo / "main.js").open("a") as source:
                source.write("\nexport function forbiddenMutation() { return 99; }\n")
            vault = fixture.root / "vault"
            vault.mkdir()
            (vault / "Sentinel.md").write_text("# Forbidden vault mutation\n")
            refused_write(fixture, "index", "--repo", fixture.repo, "--db", fixture.db, "--force")
            assert_incumbent(fixture, original_pid)
            refused_write(fixture, "brain", "refresh", vault, "--name", "backing-file-vault", "--db", fixture.db)
            assert_incumbent(fixture, original_pid)
            if digest(preserved) != before:
                raise AssertionError("a refused write changed the orphaned original graph")
            if replacement_digest is not None:
                if digest(fixture.db) != replacement_digest:
                    raise AssertionError("a refused write changed the replacement graph")
            elif fixture.db.exists():
                raise AssertionError("a refused write recreated the missing graph path")
            # The control endpoint must remain usable while health is false.
            fixture.stop_owned_daemon()
            if fixture.child.returncode != 0:
                raise AssertionError(f"unhealthy daemon did not drain cleanly: {fixture.child.returncode}")
            fixture.record(kind="backing_file_clean_drain", mutation=mutation,
                           daemon_pid=original_pid, exit_code=fixture.child.returncode,
                           passed=True)
        finally:
            # Restore exact backing identity even on assertion failure before
            # the fixture's owned-child cleanup attempts graceful shutdown.
            if not fixture.db.exists() or fixture.db.stat().st_ino != preserved.stat().st_ino:
                os.replace(preserved, fixture.db)
            if moved.exists():
                moved.unlink()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", required=True)
    arguments = parser.parse_args()
    for mutation in ("unlink", "rename", "replacement"):
        exercise(Path(arguments.binary), mutation)
    print("backing_file_admission_release_fixture: PASS", flush=True)


if __name__ == "__main__":
    main()
