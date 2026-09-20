#!/usr/bin/env python3
"""Daemon-only changed-file and test-selection regression acceptance.

Run a prebuilt binary with --binary. Every case records requests, responses,
assertions, exit codes, timing, and the owned daemon PID in retained evidence.
No build or direct database access occurs. A failing case does not hide the
remaining cases; the suite exits nonzero after recording all results.
"""
import argparse
import json
from pathlib import Path
import subprocess
import time

from isolated_daemon import IsolatedDaemon


def local(payload):
    return payload.get("local_impact", payload)


def mcp(fixture, tool, arguments):
    frames = [
        {"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
            "protocolVersion": "2024-11-05", "capabilities": {},
            "clientInfo": {"name": "release-gates", "version": "1"}}},
        {"jsonrpc": "2.0", "method": "notifications/initialized"},
        {"jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": {
            "name": tool, "arguments": arguments}},
    ]
    command = [str(fixture.binary), "mcp", "--db", str(fixture.db)]
    started = time.monotonic()
    try:
        result = subprocess.run(
            command, input="".join(json.dumps(frame) + "\n" for frame in frames),
            cwd=fixture.repo, env=fixture.env, capture_output=True, text=True,
            timeout=fixture.timeout)
    except subprocess.TimeoutExpired as error:
        fixture.record(kind="mcp_timeout", request=frames, command=command,
                       stdout=str(error.stdout or ""), stderr=str(error.stderr or ""),
                       elapsed_seconds=time.monotonic() - started,
                       daemon_pid=fixture.child.pid)
        raise
    fixture.record(kind="mcp", request=frames, command=command, stdout=result.stdout,
                   stderr=result.stderr, exit_code=result.returncode,
                   elapsed_seconds=time.monotonic() - started,
                   daemon_pid=fixture.child.pid)
    assert result.returncode == 0, result.stderr
    reply = next(frame for line in result.stdout.splitlines()
                 if (frame := json.loads(line)).get("id") == 2)
    assert "error" not in reply and not reply["result"].get("isError"), reply
    return local(reply["result"]["structuredContent"])


def cli(fixture, tool, files=None, *, empty_diff=False, limit=None):
    command = [str(fixture.binary), tool.replace("_", "-"), "--db", str(fixture.db), "--json"]
    if empty_diff:
        command.extend(["--base-ref", "HEAD"])
    elif tool == "affected_tests":
        command.extend(["--files", ",".join(files)])
    else:
        for file in files:
            command.extend(["--files", file])
    if limit is not None:
        command.extend(["--limit", str(limit)])
    started = time.monotonic()
    try:
        result = subprocess.run(command, cwd=fixture.repo, env=fixture.env,
                                capture_output=True, text=True, timeout=fixture.timeout)
    except subprocess.TimeoutExpired as error:
        fixture.record(kind="cli_timeout", request=command,
                       stdout=str(error.stdout or ""), stderr=str(error.stderr or ""),
                       elapsed_seconds=time.monotonic() - started,
                       daemon_pid=fixture.child.pid)
        raise
    fixture.record(kind="cli", request=command, stdout=result.stdout,
                   stderr=result.stderr, exit_code=result.returncode,
                   elapsed_seconds=time.monotonic() - started,
                   daemon_pid=fixture.child.pid)
    assert result.returncode == 0, (result.returncode, result.stdout, result.stderr)
    return local(json.loads(result.stdout))


def bootstrap(fixture):
    fixture.create_repository()
    files = {
        "Makefile": "all:\n\t@echo fixture\n",
        "config.yaml": "mode: fixture\n",
        "unknown.input": "opaque fixture input\n",
        "README.md": "# Fixture documentation\n",
        "untested.js": "export function untestedGateSource() { return 7; }\n",
        "covered.js": ("export function coveredGateSource() { return 9; }\n"
                       "export function coveredGateSibling() { return coveredGateSource(); }\n"),
        "covered.test.js": (
            "import { coveredGateSource } from './covered.js';\n"
            "export function coveredGateTest() { return coveredGateSource(); }\n"),
        "tests/new.test.js": "// Changed test file with no declarations.\n",
        "tests/Makefile": "test:\n\t@echo fixture\n",
    }
    for name, contents in files.items():
        path = fixture.repo / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(contents)
    # Commit only the disposable fixture so HEAD is a genuine empty diff and
    # indexing sees the exact same tracked inputs on baseline and candidate.
    for args in [("add", "."), ("-c", "user.name=Fixture", "-c",
                 "user.email=fixture@example.invalid", "commit", "-m", "gate fixture")]:
        command = ["git", "-c", f"core.hooksPath={fixture.git_empty}",
                   "-c", "commit.gpgSign=false", "-c", "tag.gpgSign=false", *args]
        result = subprocess.run(command, cwd=fixture.repo, env=fixture.env,
                                capture_output=True, text=True, timeout=30)
        fixture.record(kind="fixture_git", request=command, stdout=result.stdout,
                       stderr=result.stderr, exit_code=result.returncode)
        assert result.returncode == 0, result.stderr
    fixture.run("index", "--repo", fixture.repo, "--db", fixture.db)
    status = fixture.run("brain", "status", "--db", fixture.db, "--json")
    fixture.assert_daemon_status(status)
    fixture.record(kind="release_gate_fixture_ready", daemon_pid=fixture.child.pid,
                   graph_status=json.loads(status.stdout), files=files)


def descriptors(payload):
    return {note["descriptor"] for note in payload.get("notifications", [])}


def selected_files(payload):
    return [item for tier in ("tier_1", "tier_2", "tier_3") for item in payload[tier]]


def assert_case(payload, tool, expectation):
    assert not payload.get("refused"), payload
    assert not payload.get("resolver_stale_repos"), payload
    notes = descriptors(payload)
    if expectation in ("unassessed", "missing-source"):
        reason = "changed-file-unassessed" if expectation == "unassessed" else "changed-file-no-symbols"
        assert payload["status"] != "complete", payload
        assert reason in notes, payload
        if tool == "affected_tests":
            assert payload["recommendation"] == "run-full-suite", payload
        else:
            assert payload["gate_state"] == "degraded-unknown", payload
            risk = payload["risk"]
            assert risk.lower() != "low", payload
    elif expectation == "docs":
        assert payload["status"] == "complete", payload
        assert "docs-only-excluded" in notes, payload
        if tool == "affected_tests":
            assert not selected_files(payload), payload
            assert payload["recommendation"] == "selection-usable", payload
        else:
            assert payload["gate_state"] == "ok", payload
    elif expectation == "untested-source":
        assert payload["status"] == "complete", payload
        assert "changed-file-no-symbols" not in notes, payload
        if tool == "affected_tests":
            assert payload["changed_symbols"], payload
            assert not selected_files(payload), payload
            assert payload["recommendation"] == "run-full-suite", payload
            assert "no-tests-selected" in notes, payload
    elif expectation == "covered-source":
        assert payload["status"] == "complete", payload
        if tool == "affected_tests":
            assert any(item["test_file"].endswith("covered.test.js") for item in selected_files(payload)), payload
            assert payload["recommendation"] == "selection-usable", payload
    elif expectation == "changed-test":
        if tool == "affected_tests":
            assert payload["status"] == "complete", payload
            assert payload["recommendation"] == "selection-usable", payload
            entries = [item for item in selected_files(payload) if item["test_file"].endswith("tests/new.test.js")]
            assert len(entries) == 1 and entries[0]["tests"] == [], payload
            assert "always-include-changed-test" in notes, payload
            assert "no-tests-selected" not in notes, payload
        else:
            # Selecting a changed test file is distinct from assessing its
            # graph impact. A symbol-free test file has no assessed impact.
            assert_case(payload, tool, "missing-source")
    elif expectation == "empty-diff":
        assert payload["status"] == "complete", payload
        assert payload["changed_files"] == [], payload
        assert not selected_files(payload), payload
        assert payload["recommendation"] == "selection-usable", payload
    else:
        raise AssertionError(f"unknown expectation: {expectation}")


def cases(fixture):
    matrix = [
        ("makefile", ["Makefile"], "unassessed"),
        ("config", ["config.yaml"], "unassessed"),
        ("unknown", ["unknown.input"], "unassessed"),
        ("source-without-tests", ["untested.js"], "untested-source"),
        ("source-with-tests", ["covered.js"], "covered-source"),
        ("source-plus-makefile", ["covered.js", "Makefile"], "unassessed"),
        ("missing-source", ["missing.js"], "missing-source"),
        ("unicode-source", ["src/計算.js"], "missing-source"),
        ("documentation", ["README.md"], "docs"),
        ("changed-test-without-symbols", ["tests/new.test.js"], "changed-test"),
        ("test-directory-build-input", ["tests/Makefile"], "unassessed"),
        ("empty-git-diff", None, "empty-diff"),
    ]
    failures = []
    for name, files, expectation in matrix:
        tools = ("affected_tests",) if files is None else ("detect_changes", "blast_radius", "affected_tests")
        for tool in tools:
            # The mixed input repeats with a small display cap so truncation
            # cannot remove the missing-coverage gate or its reasons.
            limits = (None, 1) if name == "source-plus-makefile" and tool != "affected_tests" else (None,)
            for limit in limits:
                for route in ("cli", "mcp"):
                    # Refuse a lost/replaced fixture before a client can
                    # autostart another runtime and hide daemon failure.
                    if fixture.child.poll() is not None or int(fixture.pidfile.read_text().strip()) != fixture.child.pid:
                        fixture.record(kind="release_gate_daemon_ownership_lost", daemon_pid=fixture.child.pid)
                        raise RuntimeError("owned fixture daemon is no longer available")
                    started = time.monotonic()
                    identity = {"case": name, "tool": tool, "route": route, "limit": limit}
                    fixture.record(kind="release_gate_case_start", **identity)
                    try:
                        if route == "cli":
                            payload = cli(fixture, tool, files, empty_diff=files is None, limit=limit)
                        else:
                            arguments = {"base_ref": "HEAD"} if files is None else {"changed_files": files}
                            if limit is not None:
                                arguments["limit"] = limit
                            payload = mcp(fixture, tool, arguments)
                        assert_case(payload, tool, expectation)
                        assert int(fixture.pidfile.read_text().strip()) == fixture.child.pid
                        fixture.record(kind="release_gate_case", passed=True, payload=payload,
                                       elapsed_seconds=time.monotonic() - started, **identity)
                    except Exception as error:
                        failure = {**identity, "error": repr(error)}
                        failures.append(failure)
                        fixture.record(kind="release_gate_case", passed=False,
                                       elapsed_seconds=time.monotonic() - started, **failure)
    fixture.record(kind="release_change_gate_contract", passed=not failures, failures=failures,
                   daemon_pid=fixture.child.pid)
    if failures:
        raise AssertionError(f"{len(failures)} gate cases failed; evidence: {fixture.root}")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", required=True)
    args = parser.parse_args()
    fixture = IsolatedDaemon(Path(args.binary))
    print(f"Gate evidence: {fixture.root}", flush=True)
    with fixture:
        bootstrap(fixture)
        cases(fixture)
    print("release change gates: PASS", flush=True)


if __name__ == "__main__":
    main()
