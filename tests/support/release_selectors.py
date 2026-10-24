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


def mcp(fixture, tool, arguments, publication_repo=None, timeout=None):
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
                            text=True, timeout=timeout or fixture.timeout)
    fixture.record(kind="mcp", request=frames, stdout=result.stdout, stderr=result.stderr,
                   exit_code=result.returncode, daemon_pid=fixture.child.pid,
                   elapsed_seconds=time.monotonic() - started)
    assert result.returncode == 0, result.stderr
    reply = next(f for line in result.stdout.splitlines()
                 if (f := json.loads(line)).get("id") == 2)
    if publication_repo is not None:
        # Validate transport and preserve the outer compatibility envelope.
        return overlap_mcp_payload(reply, publication_repo)[1]
    assert "error" not in reply and not reply["result"].get("isError"), reply
    return local(reply["result"]["structuredContent"])


def publication_refusal(payload, active_repo_uid):
    """Only the expected local writer's typed, fully disclosed debt is transient."""
    if not isinstance(payload, dict) or "error" in payload:
        return False
    value = local(payload)
    if not isinstance(value, dict) or "error" in value:
        return False
    if value is not payload and any(
        key in payload and payload[key] != value.get(key)
        for key in ("refused", "reason", "message", "incomplete_repos")
    ):
        return False
    if value.get("refused") is not True or value.get("reason") != "repo_index_incomplete":
        return False
    if not isinstance(value.get("message"), str) or not value["message"].strip():
        return False
    if any(key in value for key in ("status", "target", "impact_nodes", "returned", "total")):
        return False
    rows = value.get("incomplete_repos")
    if not isinstance(rows, list) or len(rows) != 1 or not isinstance(rows[0], dict):
        return False
    row = rows[0]
    if row.get("repo_uid") != active_repo_uid:
        return False
    if not all(isinstance(row.get(key), str) and row[key].strip() for key in ("url", "remedy")):
        return False
    metas = [item["_meta"] for item in (payload, value) if "_meta" in item]
    if not metas:
        return False
    for meta in metas:
        if not isinstance(meta, dict) or meta.get("scope") != "local":
            return False
        compatibility = meta.get("index_compatibility")
        if not isinstance(compatibility, dict) or compatibility.get("scope") != "visible_repositories":
            return False
        if compatibility.get("incomplete_repos") != rows:
            return False
    return True


def current_limited_impact(payload):
    value = local(payload)
    assert isinstance(payload, dict) and isinstance(value, dict), payload
    assert "error" not in payload and "error" not in value, payload
    assert not payload.get("refused") and not value.get("refused"), payload
    metas = [item["_meta"] for item in (payload, value) if "_meta" in item]
    assert metas, payload
    for meta in metas:
        assert isinstance(meta, dict) and meta.get("scope") == "local", payload
        compatibility = meta.get("index_compatibility")
        assert isinstance(compatibility, dict), payload
        assert compatibility.get("scope") == "visible_repositories", payload
        assert compatibility.get("incomplete_repos") == [], payload
        assert compatibility.get("resolver_incompatible_repos") == [], payload
        assert compatibility.get("vault_refresh_required") == [], payload
    assert value.get("status") == "ok", payload
    assert value["returned"] == 1 and value["total"] >= 2, payload
    assert value["truncated_by_limit"] is True, payload
    assert "truncated_by_threshold" in value and "truncated_by_depth" in value, payload


def overlap_cli_payload(result, active_repo_uid):
    payload = json.loads(result.stdout)
    if result.returncode == 2 and publication_refusal(payload, active_repo_uid):
        return "refused", payload
    assert result.returncode == 0, (result.returncode, result.stdout, result.stderr)
    current_limited_impact(payload)
    return "current", payload


def overlap_mcp_payload(reply, active_repo_uid):
    assert isinstance(reply, dict) and "error" not in reply, reply
    body = reply["result"]
    payload = body["structuredContent"]
    if publication_refusal(payload, active_repo_uid):
        return "refused", payload
    assert not body.get("isError"), reply
    current_limited_impact(payload)
    return "current", payload


def overlap_predicate_controls():
    """Malformed failures and invalid successes never become retryable outcomes."""
    uid = "repo:control"
    row = {"repo_uid": uid, "url": "file:///control", "remedy": "Re-index this repository"}
    good = {"refused": True, "reason": "repo_index_incomplete", "message": "Index incomplete",
            "incomplete_repos": [row], "_meta": {"scope": "local", "index_compatibility": {
                "scope": "visible_repositories", "incomplete_repos": [row]}}}
    assert publication_refusal(good, uid)
    wrapped = {"local_impact": {key: value for key, value in good.items() if key != "_meta"},
               "_meta": good["_meta"]}
    assert publication_refusal(wrapped, uid)
    current = {"status": "ok", "returned": 1, "total": 2, "truncated_by_limit": True,
               "truncated_by_threshold": False, "truncated_by_depth": False,
               "_meta": {"scope": "local", "index_compatibility": {
                   "scope": "visible_repositories", "incomplete_repos": [],
                   "resolver_incompatible_repos": [], "vault_refresh_required": []}}}
    current_wrapped = {"local_impact": {key: value for key, value in current.items() if key != "_meta"},
                       "_meta": current["_meta"]}
    for value in (good, wrapped):
        result = subprocess.CompletedProcess([], 2, json.dumps(value), "")
        assert overlap_cli_payload(result, uid) == ("refused", value)
        reply = {"result": {"isError": True, "structuredContent": value}}
        assert overlap_mcp_payload(reply, uid) == ("refused", value)
    for value in (current, current_wrapped):
        result = subprocess.CompletedProcess([], 0, json.dumps(value), "")
        assert overlap_cli_payload(result, uid) == ("current", value)
        reply = {"result": {"structuredContent": value}}
        assert overlap_mcp_payload(reply, uid) == ("current", value)
    mutations = [
        lambda value: value.update(refused=False),
        lambda value: value.update(refused=1),
        lambda value: value.update(reason="not_found"),
        lambda value: value.update(message=" "),
        lambda value: value.update(incomplete_repos=[]),
        lambda value: value["incomplete_repos"][0].update(repo_uid="repo:other"),
        lambda value: value["incomplete_repos"][0].update(remedy=""),
        lambda value: value["_meta"]["index_compatibility"].update(incomplete_repos=[]),
        lambda value: value["_meta"]["index_compatibility"].update(scope="requested_repositories"),
        lambda value: value["_meta"].update(scope="org"),
        lambda value: value.pop("_meta"),
        lambda value: value.update(status="ok", returned=0, total=0),
        lambda value: value.update(error="unrelated failure"),
    ]
    for mutate in mutations:
        bad = json.loads(json.dumps(good))
        mutate(bad)
        assert not publication_refusal(bad, uid), bad
    for payload in (None, [], {"status": "not_found"}, {"error": "permission denied"}):
        assert not publication_refusal(payload, uid), payload
    for code, value in ((0, good), (1, good), (2, {"status": "not_found"}),
                        (2, {"error": "permission denied"}), (0, {"status": "ok", "returned": 0})):
        result = subprocess.CompletedProcess([], code, json.dumps(value), "")
        try:
            overlap_cli_payload(result, uid)
        except (AssertionError, KeyError, TypeError, ValueError):
            continue
        raise AssertionError(("unexpected overlap classification", code, value))
    inconsistent = json.loads(json.dumps(current))
    inconsistent["_meta"]["index_compatibility"]["incomplete_repos"] = [row]
    outer_error = {**current_wrapped, "error": "unrelated outer failure"}
    for value in (inconsistent, outer_error):
        try:
            overlap_cli_payload(subprocess.CompletedProcess([], 0, json.dumps(value), ""), uid)
        except (AssertionError, KeyError, TypeError, ValueError):
            pass
        else:
            raise AssertionError(("invalid current answer accepted", value))
    for reply in ({"error": {"code": -32603}, "result": {"structuredContent": good}},
                  {"result": {"isError": True, "structuredContent": current}},
                  {"result": {}},
                  {"result": {"structuredContent": {"status": "not_found"}}}):
        try:
            overlap_mcp_payload(reply, uid)
        except (AssertionError, KeyError, TypeError, ValueError):
            pass
        else:
            raise AssertionError(("invalid MCP transport/payload accepted", reply))
    contradictory = {**wrapped, "refused": False}
    assert not publication_refusal(contradictory, uid)
    try:
        overlap_cli_payload(subprocess.CompletedProcess([], 2, "not JSON", ""), uid)
    except ValueError:
        pass
    else:
        raise AssertionError("malformed JSON was accepted")


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
    # Only this repository is mutated; any debt on another visible repo is a failure.
    repos = json.loads(fixture.run("list-repos", "--db", fixture.db, "--json").stdout)
    active_repo = next(repo for repo in repos if Path(repo["root_path"]) == fixture.repo)
    active_repo_uid = active_repo["uid"]
    owned_pid = fixture.child.pid
    for number in range(300):
        (fixture.repo / f"writer{number}.js").write_text(
            f"export function writer{number}() {{ return {number}; }}\n")
    stop = threading.Event()
    errors = []
    outcomes = []

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
        # A complete current answer or an exact fail-closed publication refusal
        # is valid while the writer owns a publication. Never retry assertions.
        filters = [[], ["--repo", "repo"], ["--confidence", "0.1"],
                   ["--min-score", "0"], ["--repo", "repo", "--confidence", "0.1"],
                   ["--repo", "repo", "--min-score", "0"],
                   ["--confidence", "0.1", "--min-score", "0"],
                   ["--repo", "repo", "--confidence", "0.1", "--min-score", "0"]]
        deadline = time.monotonic() + fixture.timeout
        for route in ("cli", "mcp"):
            for flags in filters:
                remaining = deadline - time.monotonic()
                assert remaining > 0, "selector overlap exceeded its bounded deadline"
                if errors:
                    raise errors[0]
                if route == "cli":
                    command = [str(fixture.binary), "impact", "releaseTarget", "--db", str(fixture.db),
                               "--json", *flags, "--limit", "1"]
                    started = time.monotonic()
                    result = subprocess.run(command, cwd=fixture.root, env=fixture.env,
                                            capture_output=True, text=True, timeout=remaining)
                    fixture.record(kind="cli", request=command, stdout=result.stdout,
                                   stderr=result.stderr, exit_code=result.returncode,
                                   elapsed_seconds=time.monotonic() - started, daemon_pid=owned_pid)
                    outcome, payload = overlap_cli_payload(result, active_repo_uid)
                else:
                    arguments = {"symbol": "releaseTarget", "limit": 1}
                    for key, value in zip(flags[::2], flags[1::2]):
                        arguments[key[2:].replace("-", "_")] = value if key == "--repo" else float(value)
                    payload = mcp(fixture, "brain_impact", arguments,
                                  publication_repo=active_repo_uid, timeout=remaining)
                    if publication_refusal(payload, active_repo_uid):
                        outcome = "refused"
                    else:
                        current_limited_impact(payload)
                        outcome = "current"
                assert fixture.child.poll() is None and int(fixture.pidfile.read_text()) == owned_pid
                outcomes.append({"route": route, "flags": flags, "outcome": outcome})
                fixture.record(kind="release_selector_during_publication", route=route,
                               flags=flags, outcome=outcome, payload=payload, daemon_pid=owned_pid)
    finally:
        stop.set()
        thread.join(fixture.timeout + 5)
        if thread.is_alive():
            raise RuntimeError("index request did not drain before cleanup")
    if errors:
        raise errors[0]
    # The unchanged full table must pass ONCE after all writer requests finish.
    # Persistent debt, malformed failures and selector regressions are never retried.
    cases(fixture)
    state = json.loads(fixture.run("brain", "status", "--db", fixture.db, "--json").stdout)
    assert state["index_compatibility"]["incomplete_repos"] == [], state
    assert not state["index_publication"]["dirty"], state
    assert fixture.child.poll() is None and int(fixture.pidfile.read_text()) == owned_pid
    fixture.record(kind="release_selectors_use_daemon_during_index", passed=True,
                   daemon_pid=owned_pid, outcomes=outcomes, writer_drained=True,
                   post_drain_full_table=True, incomplete_repos=[])


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", required=True)
    parser.add_argument("--during-index", action="store_true")
    args = parser.parse_args()
    overlap_predicate_controls()
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
