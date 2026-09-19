#!/usr/bin/env python3
"""Daemon-only selector acceptance, launched by the serialized release controller.

No build, direct database access, or bypass request. Evidence is retained by
IsolatedDaemon. --during-index additionally overlaps reads with daemon indexing.
"""
import argparse
import json
from pathlib import Path
import subprocess
import threading
import time

from isolated_daemon import IsolatedDaemon


def local(payload):
    return payload.get("local_impact", payload)


def mcp(fixture, tool, arguments):
    frames = [
        {"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
            "protocolVersion": "2024-11-05", "capabilities": {},
            "clientInfo": {"name": "release-selectors", "version": "1"}}},
        {"jsonrpc": "2.0", "method": "notifications/initialized"},
        {"jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": {
            "name": tool, "arguments": arguments}},
    ]
    command = [str(fixture.binary), "mcp", "--db", str(fixture.db)]
    started = time.monotonic()
    result = subprocess.run(command, input="".join(json.dumps(f) + "\n" for f in frames),
                            cwd=fixture.root, env=fixture.env, capture_output=True,
                            text=True, timeout=fixture.timeout)
    fixture.record(kind="mcp", request=frames, stdout=result.stdout, stderr=result.stderr,
                   exit_code=result.returncode, daemon_pid=fixture.child.pid,
                   elapsed_seconds=time.monotonic() - started)
    assert result.returncode == 0, result.stderr
    reply = next(f for line in result.stdout.splitlines()
                 if (f := json.loads(line)).get("id") == 2)
    assert "error" not in reply and not reply["result"].get("isError"), reply
    return local(reply["result"]["structuredContent"])


def missing_database_reads(fixture):
    # A read against an uncreated DB must neither spawn an owner nor create a
    # store. This runs before IsolatedDaemon.start, separately from startup.
    assert not fixture.db.exists()
    for command in (("impact", "releaseTarget"), ("brain", "status")):
        result = fixture.run(*command, "--db", fixture.db, "--json", expected=1)
        assert "database" in (result.stdout + result.stderr).lower(), result
        assert not fixture.db.exists(), "read created a database"
        assert not fixture.pidfile.exists(), "read started a daemon"
    fixture.record(kind="release_missing_database_reads_do_not_start_daemon", passed=True)


def bootstrap(fixture):
    fixture.bootstrap()
    primary = fixture.repo
    with (primary / "main.js").open("a") as output:
        output.write("export function sharedName() { return 1; }\n"
                     "export function secondCaller() { return releaseTarget('second'); }\n")
    fixture.run("index", "--repo", primary, "--db", fixture.db)
    fixture.repo = fixture.root / "other"
    fixture.create_repository()
    (fixture.repo / "main.js").write_text(
        "export function sharedName() { return 2; }\n"
        "export function otherTarget() { return sharedName(); }\n")
    fixture.run("index", "--repo", fixture.repo, "--db", fixture.db)
    fixture.repo = primary


def cases(fixture):
    def impact(name="releaseTarget", *flags, expected=0):
        return local(json.loads(fixture.run("impact", name, "--db", fixture.db,
                                           "--json", *flags, expected=expected).stdout))

    filters = [[], ["--repo", "repo"], ["--confidence", "0.1"],
               ["--min-score", "0"], ["--repo", "repo", "--confidence", "0.1"],
               ["--repo", "repo", "--min-score", "0"],
               ["--confidence", "0.1", "--min-score", "0"],
               ["--repo", "repo", "--confidence", "0.1", "--min-score", "0"]]
    for flags in filters:
        payload = impact("releaseTarget", *flags, "--limit", "1")
        assert payload["status"] == "ok", payload
        assert payload["returned"] == 1 and payload["total"] >= 2, payload
        assert payload["truncated_by_limit"] is True, payload
        assert "truncated_by_threshold" in payload and "truncated_by_depth" in payload
    assert impact("releaseTarget", "--repo", "other", expected=2)["status"] == "not_found"
    assert impact("sharedName", expected=3)["status"] == "ambiguous"
    assert impact("missingName", expected=2)["status"] == "not_found"
    assert impact("sharedName", "--repo", "other")["status"] == "ok"
    uid = impact()["target"]
    assert impact(uid, "--repo", "other", expected=2)["status"] == "not_found"
    assert impact(uid)["status"] == "ok"
    for name in ("releaseTarget", uid):
        payload = json.loads(fixture.run("cross-repo-refs", name, "--repo", "other",
                                         "--limit", "1", "--db", fixture.db, "--json").stdout)
        assert payload["uid"] == uid and payload["returned"] <= 1, payload
    ambiguous = fixture.run("cross-repo-refs", "sharedName", "--db", fixture.db,
                            "--json", expected=3)
    assert json.loads(ambiguous.stdout)["status"] == "ambiguous"
    fixture.run("cross-repo-refs", "sharedName", "--repo", "missingRepo", "--db",
                fixture.db, "--json", expected=2)
    fixture.run("cross-repo-refs", "missingName", "--db", fixture.db, "--json", expected=2)
    for repo in ("repo", "other"):
        fixture.run("cross-repo-refs", "sharedName", "--repo", repo, "--limit", "1",
                    "--db", fixture.db, "--json")
    for flags in filters:
        args = {"symbol": "releaseTarget", "limit": 1}
        for key, value in zip(flags[::2], flags[1::2]):
            args[key[2:].replace("-", "_")] = value if key == "--repo" else float(value)
        payload = mcp(fixture, "brain_impact", args)
        assert payload["status"] == "ok" and payload["returned"] == 1, payload
        assert payload["truncated_by_limit"] is True, payload
    payload = mcp(fixture, "cross_repo_contracts", {"name": "releaseTarget", "name_repo": "other", "limit": 1})
    assert payload["uid"] == uid and payload["returned"] <= 1, payload
    assert int(fixture.pidfile.read_text().strip()) == fixture.child.pid
    fixture.record(kind="release_unique_ref_uid_survives_disambiguator", passed=True)
    fixture.record(kind="release_selector_preserves_limit", passed=True)


def during_index(fixture):
    # Repeated daemon-owned writes keep the overlap window open across all
    # CLI/MCP reads. The writer finishes its current request before cleanup.
    for number in range(300):
        (fixture.repo / f"writer{number}.js").write_text(
            f"export function writer{number}() {{ return {number}; }}\n")
    stop = threading.Event()
    errors = []

    def writer():
        try:
            while not stop.is_set():
                fixture.run("index", "--repo", fixture.repo, "--db", fixture.db, "--force")
        except BaseException as error:
            errors.append(error)

    thread = threading.Thread(target=writer)
    thread.start()
    try:
        deadline = time.monotonic() + fixture.timeout
        while time.monotonic() < deadline:
            state = json.loads(fixture.run("brain", "status", "--db", fixture.db, "--json").stdout)
            if state.get("indexing_active"):
                break
            if errors:
                raise errors[0]
            time.sleep(0.1)
        else:
            raise AssertionError("writer never reported indexing_active; overlap was not established")
        cases(fixture)
        fixture.record(kind="release_selectors_use_daemon_during_index", passed=True,
                       daemon_pid=fixture.child.pid)
    finally:
        stop.set()
        thread.join(fixture.timeout + 5)
        if thread.is_alive():
            raise RuntimeError("index request did not drain before cleanup")
    if errors:
        raise errors[0]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", required=True)
    parser.add_argument("--during-index", action="store_true")
    args = parser.parse_args()
    fixture = IsolatedDaemon(Path(args.binary))
    print(f"Selector evidence: {fixture.root}", flush=True)
    missing_database_reads(fixture)
    with fixture:
        bootstrap(fixture)
        cases(fixture)
        if args.during_index:
            during_index(fixture)
    print("release selectors: PASS", flush=True)


if __name__ == "__main__":
    main()
