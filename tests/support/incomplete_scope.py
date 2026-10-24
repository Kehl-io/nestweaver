#!/usr/bin/env python3
"""Exercise standard CLI/MCP incomplete-index refusals using an isolated seed.

The supported Rust release test prepares the seed with a complete selected
repository A and a visible incomplete repository B, then closes its GraphStore.
This script only moves that store and drives the real daemon; it never opens it.
"""
import argparse
import json
import os
from pathlib import Path
import subprocess
import time

from isolated_daemon import IsolatedDaemon


def payload_local(value):
    return value.get("local_impact", value)


def assert_refusal(value, incomplete_repo):
    value = payload_local(value)
    if value.get("refused") is not True or value.get("reason") != "repo_index_incomplete":
        raise AssertionError(f"expected incomplete-index refusal: {value}")
    debts = value.get("incomplete_repos", [])
    if not any(debt.get("repo_uid") == incomplete_repo
               and "re-index" in debt.get("remedy", "").lower() for debt in debts):
        raise AssertionError(f"refusal omitted the incomplete repository or remedy: {value}")
    if not value.get("message"):
        raise AssertionError(f"refusal omitted a readable explanation: {value}")


def owned(fixture):
    if fixture.child.poll() is not None or int(fixture.pidfile.read_text().strip()) != fixture.child.pid:
        raise AssertionError("a refusal request replaced or stopped the owned daemon")


def mcp_refusals(fixture, cases, incomplete_repo):
    frames = [
        {"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
            "protocolVersion": "2024-11-05", "capabilities": {},
            "clientInfo": {"name": "incomplete-scope", "version": "1"}}},
        {"jsonrpc": "2.0", "method": "notifications/initialized"},
    ]
    for identifier, (tool, arguments) in enumerate(cases, 2):
        frames.append({"jsonrpc": "2.0", "id": identifier, "method": "tools/call",
                       "params": {"name": tool, "arguments": arguments}})
    command = [str(fixture.binary), "mcp", "--db", str(fixture.db)]
    started = time.monotonic()
    result = subprocess.run(command, cwd=fixture.root, env=fixture.env,
                            input="".join(json.dumps(frame) + "\n" for frame in frames),
                            capture_output=True, text=True, timeout=fixture.timeout)
    fixture.record(kind="incomplete_scope_mcp", request=frames, stdout=result.stdout,
                   stderr=result.stderr, exit_code=result.returncode,
                   elapsed_seconds=time.monotonic() - started, daemon_pid=fixture.child.pid)
    if result.returncode != 0:
        raise AssertionError(f"MCP session failed: {result}")
    replies = {}
    for line in result.stdout.splitlines():
        frame = json.loads(line)
        if "id" in frame:
            if frame["id"] in replies:
                raise AssertionError(f"duplicate MCP reply: {frame}")
            replies[frame["id"]] = frame
    for identifier, (tool, _arguments) in enumerate(cases, 2):
        reply = replies.get(identifier)
        if reply is None or "error" in reply:
            raise AssertionError(f"{tool}: missing structured refusal: {reply}")
        body = reply.get("result", {})
        value = body.get("structuredContent")
        if value is None:
            text = "".join(part.get("text", "") for part in body.get("content", [])
                           if part.get("type") == "text")
            value = json.loads(text)
        assert_refusal(value, incomplete_repo)
    owned(fixture)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", required=True)
    parser.add_argument("--seed-db", required=True)
    parser.add_argument("--repo-a", required=True)
    parser.add_argument("--repo-b", required=True)
    parser.add_argument("--symbol", required=True)
    args = parser.parse_args()
    seed = Path(args.seed_db).resolve(strict=True)
    fixture = IsolatedDaemon(args.binary)
    print(f"Incomplete-scope artifacts: {fixture.root}", flush=True)
    # Rename the database and inode-bound marker together, after Rust closed
    # its store. Keep process locks behind; no live owner's files are copied.
    for suffix in ("", ".engine-format", ".generation", ".resolver_generation.json"):
        source = Path(str(seed) + suffix)
        if source.exists():
            os.rename(source, Path(str(fixture.db) + suffix))
    with fixture:
        # Do not bootstrap/index: that would heal the deliberately seeded debt.
        cli_cases = [
            ("impact", args.symbol, "--repo", args.repo_a),
            ("flow-trace", args.symbol, "--repo", args.repo_a),
            ("blast-radius", "--files", "src/complete.js", "--repo", args.repo_a),
            ("dead-code", "--repo", args.repo_a),
            ("affected-tests", "--files", "src/complete.js", "--repo", args.repo_a),
        ]
        for command in cli_cases:
            for structured in (False, True):
                flags = ["--json"] if structured else []
                result = fixture.run(*command, "--db", fixture.db, *flags, expected=2)
                if structured:
                    assert_refusal(json.loads(result.stdout), args.repo_b)
                else:
                    message = (result.stdout + result.stderr).lower()
                    if not any(reason in message for reason in ("incomplete", "did not complete")) or "re-index" not in message:
                        raise AssertionError(f"plain refusal omitted reason/remedy: {result}")
                owned(fixture)
        mcp_refusals(fixture, [
            ("brain_impact", {"symbol": args.symbol, "repo": args.repo_a}),
            ("flow_trace", {"symbol": args.symbol, "repo": args.repo_a}),
            ("blast_radius", {"changed_files": ["src/complete.js"], "repo": args.repo_a}),
            ("dead_code", {"repos": [args.repo_a]}),
            ("affected_tests", {"changed_files": ["src/complete.js"], "repo": args.repo_a}),
        ], args.repo_b)
        fixture.record(kind="incomplete_scope_release_fixture", passed=True,
                       complete_seed_repo=args.repo_a, incomplete_visible_repo=args.repo_b)
    print("incomplete_scope_release_fixture: PASS", flush=True)


if __name__ == "__main__":
    main()
