#!/usr/bin/env python3
"""Review-only dead-code and bounded-page checks through owned isolated daemons.

No builds, direct database calls or production changes. The release controller
supplies an already-built standard artifact. This is a small contract fixture,
not the required historical top-15 or full C++ corpus measurement.
"""
import argparse
import json
import subprocess
import time

from isolated_daemon import IsolatedDaemon


def mcp(fixture, arguments):
    frames = [
        {"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
            "protocolVersion": "2024-11-05", "capabilities": {},
            "clientInfo": {"name": "release-dead-code", "version": "1"}}},
        {"jsonrpc": "2.0", "method": "notifications/initialized"},
        {"jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": {
            "name": "dead_code", "arguments": arguments}},
    ]
    started = time.monotonic()
    result = subprocess.run([str(fixture.binary), "mcp", "--db", str(fixture.db)],
                            input="".join(json.dumps(frame) + "\n" for frame in frames),
                            cwd=fixture.root, env=fixture.env, capture_output=True,
                            text=True, timeout=fixture.timeout)
    fixture.record(kind="mcp_dead_code", request=frames, stdout=result.stdout,
                   stderr=result.stderr, exit_code=result.returncode,
                   elapsed_seconds=time.monotonic() - started, daemon_pid=fixture.child.pid)
    assert result.returncode == 0, result.stderr
    reply = next(frame for line in result.stdout.splitlines()
                 if (frame := json.loads(line)).get("id") == 2)
    assert "error" not in reply and not reply["result"].get("isError"), reply
    return reply["result"]["structuredContent"]


def cli(fixture, scope="repo", limit=3, offset=0, generation=None, token=None,
        confidence="low", expected=0):
    args = ["dead-code", "--db", fixture.db, "--repo", scope, "--limit", str(limit),
            "--offset", str(offset), "--min-confidence", confidence, "--json"]
    if generation is not None:
        args.extend(["--generation", str(generation)])
    if token is not None:
        args.extend(["--page-token", token])
    return json.loads(fixture.run(*args, expected=expected).stdout)


def refused(payload):
    assert payload.get("refused") is True, payload
    assert "unreachable_symbols" not in payload, payload


def prepare(fixture):
    fixture.bootstrap()
    source = (
        "function exportedValue() { return retainedLeaf(); }\n"
        "function retainedLeaf() { return 1; }\n"
        "function defaultValue() { return retainedDefaultLeaf(); }\n"
        "function retainedDefaultLeaf() { return 2; }\n"
        "export { exportedValue as renamed, defaultValue as default };\n"
    )
    source += "".join(f"function _candidate{n:02d}() {{ return {n}; }}\n" for n in range(17))
    (fixture.repo / "library.js").write_text(source)
    fixture.run("index", "--repo", fixture.repo, "--db", fixture.db)
    original_repo = fixture.repo
    fixture.repo = fixture.root / "other"
    fixture.create_repository()
    (fixture.repo / "other-library.js").write_text("function _otherCandidate() { return 1; }\n")
    fixture.run("index", "--repo", fixture.repo, "--db", fixture.db)
    fixture.repo = original_repo


def cases(fixture):
    first = cli(fixture)
    assert first.get("review_only") is True and first["high_confidence_available"] is False, first
    assert first["paging_scope"] == "local_database", first
    assert first["next_offset"] is not None, first
    generation, token = first["graph_generation"], first["page_token"]
    collected = list(first["unreachable_symbols"])
    page = first
    for _ in range(30):
        if page["next_offset"] is None:
            break
        page = cli(fixture, offset=page["next_offset"], generation=generation, token=token)
        assert page["page_token"] == token and page["graph_generation"] == generation, page
        collected.extend(page["unreachable_symbols"])
    else:
        raise AssertionError("fixture exceeded bounded 30-page walk")
    uids = [row["uid"] for row in collected]
    assert len(uids) == len(set(uids)) == first["matching_count"], collected
    names = {row["name"] for row in collected}
    assert {f"_candidate{n:02d}" for n in range(17)} <= names, names
    assert not names.intersection({"exportedValue", "retainedLeaf", "defaultValue", "retainedDefaultLeaf", "_otherCandidate"}), names
    assert all(row["confidence"] in ("low", "medium") for row in collected), collected
    full = cli(fixture, limit=1000)
    assert uids == [row["uid"] for row in full["unreachable_symbols"]], full
    same = mcp(fixture, {"repos": ["repo"], "limit": 3})
    assert same["page_token"] == token and same["unreachable_symbols"] == first["unreachable_symbols"], same
    second = mcp(fixture, {"repos": ["repo"], "limit": 3, "offset": first["next_offset"],
                           "expected_generation": generation, "page_token": token})
    assert second["unreachable_symbols"] == collected[3:6], second
    for payload in (cli(fixture, confidence="high"), mcp(fixture, {"repos": ["repo"], "min_confidence": "high"})):
        assert payload["returned"] == 0 and payload["unreachable_count"] > 0, payload
        assert payload["confidence_filter_status"] == "unavailable_no_validated_population", payload
        assert payload["requested_min_confidence"] == "high", payload
    text = fixture.run("dead-code", "--db", fixture.db, "--min-confidence", "high").stdout
    assert "High confidence is unavailable" in text, text
    assert "No dead code detected" not in text and "all reachable" not in text, text
    refused(cli(fixture, offset=3, expected=2))
    refused(cli(fixture, scope="other", offset=3, generation=generation, token=token, expected=2))
    refused(cli(fixture, confidence="medium", offset=3, generation=generation, token=token, expected=2))
    refused(mcp(fixture, {"repos": ["other"], "offset": 3, "expected_generation": generation, "page_token": token}))
    (fixture.repo / "added-library.js").write_text("function _addedCandidate() { return 1; }\n")
    fixture.run("index", "--repo", fixture.repo, "--db", fixture.db)
    refused(cli(fixture, offset=3, generation=generation, token=token, expected=2))
    refused(mcp(fixture, {"repos": ["repo"], "offset": 3, "expected_generation": generation, "page_token": token}))
    assert int(fixture.pidfile.read_text()) == fixture.child.pid
    fixture.record(kind="release_dead_code_review_pages", passed=True,
                   first_generation=generation, first_page_token=token,
                   full_uid_set=uids, returned=len(uids), daemon_pid=fixture.child.pid)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", required=True)
    args = parser.parse_args()
    fixture = IsolatedDaemon(args.binary)
    print(f"Dead-code contract evidence: {fixture.root}", flush=True)
    with fixture:
        prepare(fixture)
        cases(fixture)
    print("release_dead_code_review_pages: PASS", flush=True)


if __name__ == "__main__":
    main()
