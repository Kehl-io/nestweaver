#!/usr/bin/env python3
"""Authenticated MCP HTTP acceptance through one owned, isolated daemon.

Requires a prebuilt binary. Never opens a store, builds, reuses an external
server, or edits live artifacts. Evidence and disposable inputs are retained.
"""
import argparse
import http.client
import itertools
import json
from pathlib import Path
import secrets
import subprocess
import time

from isolated_daemon import IsolatedDaemon


SECTIONS = ("auth", "selectors", "gates", "unavailable", "context", "notes")
MAX_RESPONSE_BYTES = 8 * 1024 * 1024


class RouteDaemon(IsolatedDaemon):
    def __init__(self, binary, timeout=120):
        super().__init__(binary, timeout)
        self.repo = self.root / "visible_repo"
        self.hidden_repo = self.root / "hidden_repo"
        self.tokens = {name: secrets.token_urlsafe(36) for name in ("query", "admin")}
        self.env["NESTWEAVER_AUTH_TOKEN"] = self.tokens["query"]
        self.env["NESTWEAVER_ADMIN_TOKEN"] = self.tokens["admin"]
        self.port_file = self.root / "ports"
        self.config = self.root / "instance.toml"
        self.models = self.root / "models"
        self.models.mkdir()
        self.config.write_text(f'''instance_id = "default"
repos = []
[snapshot_storage]
backend = "local"
path = {json.dumps(str(self.root / "snapshots"))}
[workspace]
backend = "local"
path = {json.dumps(str(self.root / "workspace"))}
[inference]
endpoint = "http://127.0.0.1:1"
embedding_model = "unused"
summary_model = "unused"
[git]
credential_method = "gh"
[embedding]
cache_dir = {json.dumps(str(self.models))}
accelerator = "cpu"
auto_repair_cache = false
[authz.rules]
{json.dumps(self.tokens["query"])} = [{json.dumps(self.repo.as_uri())}]
''')
        self.config.chmod(0o600)
        self.sessions = {}
        self.request_ids = itertools.count(1)
        self.failures = []
        self.hidden_markers = set()

    def owned(self):
        if (self.child is None or self.child.poll() is not None
                or not self.pidfile.exists()
                or int(self.pidfile.read_text().strip()) != self.child.pid):
            raise RuntimeError("owned fixture daemon is unavailable or replaced")

    def run(self, *args, **kwargs):
        if self.child is not None:
            self.owned()
        result = super().run(*args, **kwargs)
        if self.child is not None:
            self.owned()
        return result

    def spawn_owned_child(self, command):
        command = [
            *command, "--server", "--bind", "127.0.0.1:0",
            "--port-file", str(self.port_file), "--config", str(self.config)]
        super().spawn_owned_child(command)
        self.record(kind="release_route_daemon_start", request=command, daemon_pid=self.child.pid)

    def start(self):
        super().start()
        deadline = time.monotonic() + self.timeout
        while time.monotonic() < deadline:
            self.owned()
            if self.port_file.exists():
                ports = self.port_file.read_text().splitlines()
                if len(ports) == 2 and all(p.isdecimal() for p in ports):
                    self.grpc_port, self.mcp_port = map(int, ports)
                    assert 0 < self.grpc_port <= 65535 and 0 < self.mcp_port <= 65535
                    assert self.grpc_port != self.mcp_port
                    break
            time.sleep(0.1)
        else:
            raise TimeoutError("owned daemon did not publish both listener ports")
        for identity in self.tokens:
            # The port file precedes spawning the listener task. Retry only
            # connection refusal during that startup window, never tool errors.
            while True:
                try:
                    status, headers, body = self.http(identity, "initialize", {
                        "protocolVersion": "2024-11-05", "capabilities": {},
                        "clientInfo": {"name": "release-routes", "version": "1"}})
                    break
                except ConnectionRefusedError:
                    self.owned()
                    if time.monotonic() >= deadline:
                        raise
                    time.sleep(0.1)
            assert status == 200 and "error" not in body, body
            assert body["result"]["serverInfo"]["name"] == "nestweaver-brain", body
            self.sessions[identity] = headers["mcp-session-id"]
        self.record(kind="release_route_listener_ready", daemon_pid=self.child.pid,
                    grpc_port=self.grpc_port, mcp_port=self.mcp_port,
                    visible_repo_url=self.repo.as_uri(), model_cache=str(self.models))
        return self

    def http(self, identity, method, params=None):
        self.owned()
        request_id = next(self.request_ids)
        frame = {"jsonrpc": "2.0", "id": request_id, "method": method}
        if params is not None:
            frame["params"] = params
        headers = {"Content-Type": "application/json", "Accept": "application/json"}
        if identity in self.tokens:
            headers["Authorization"] = "Bearer " + self.tokens[identity]
        elif identity == "wrong":
            headers["Authorization"] = "Bearer fixture-invalid-credential"
        if identity in self.sessions:
            headers["Mcp-Session-Id"] = self.sessions[identity]
        started = time.monotonic()
        connection = http.client.HTTPConnection("127.0.0.1", self.mcp_port, timeout=self.timeout)
        status, response_headers, raw = None, {}, b""
        try:
            connection.request("POST", "/mcp", body=json.dumps(frame), headers=headers)
            response = connection.getresponse()
            status = response.status
            response_headers = {key.lower(): value for key, value in response.getheaders()}
            # HTTPConnection connects directly to this loopback port. It never
            # consults proxy variables or follows a Location header.
            assert not 300 <= status < 400, "fixture HTTP redirects are forbidden"
            raw = response.read(MAX_RESPONSE_BYTES + 1)
            assert len(raw) <= MAX_RESPONSE_BYTES, "HTTP response exceeded fixture bound"
            body = json.loads(raw)
            assert body.get("jsonrpc") == "2.0", body
            if status == 200:
                assert body.get("id") == request_id, body
        except Exception as error:
            self.record(kind="release_route_http", identity=identity, request=frame,
                        status=status, body=raw.decode(errors="replace"), passed=False,
                        error=repr(error), elapsed_seconds=time.monotonic() - started,
                        daemon_pid=self.child.pid)
            raise
        finally:
            connection.close()
        self.owned()
        self.record(kind="release_route_http", identity=identity, request=frame,
                    status=status, response=body,
                    headers={k: response_headers[k] for k in ("content-type", "retry-after", "cache-control")
                             if k in response_headers},
                    elapsed_seconds=time.monotonic() - started, daemon_pid=self.child.pid)
        return status, response_headers, body

    def call(self, identity, tool, arguments, *, error=None):
        status, _, body = self.http(identity, "tools/call", {"name": tool, "arguments": arguments})
        assert status == 200 and "error" not in body, body
        result = body["result"]
        if error is not None:
            assert result.get("isError") is True, body
            message = "\n".join(item.get("text", "") for item in result.get("content", []))
            assert error.lower() in message.lower(), body
            assert not result.get("structuredContent"), body
            return result
        assert result.get("isError") is False, body
        payload = result["structuredContent"]
        assert isinstance(payload, dict), payload
        return payload.get("local_impact", payload)

    def no_hidden(self, payload):
        serialized = json.dumps(payload)
        for marker in self.hidden_markers:
            assert marker not in serialized, ("hidden data disclosed", marker, payload)

    def case(self, section, name, operation):
        self.owned()
        started = time.monotonic()
        try:
            operation()
            self.owned()
        except Exception as error:
            failure = {"section": section, "case": name, "error": repr(error)}
            self.failures.append(failure)
            self.record(kind="release_route_case", passed=False, **failure,
                        elapsed_seconds=time.monotonic() - started)
        else:
            self.record(kind="release_route_case", section=section, case=name, passed=True,
                        elapsed_seconds=time.monotonic() - started)


def bootstrap(fixture):
    visible = fixture.repo
    for repo, prefix in ((visible, "visible"), (fixture.hidden_repo, "hidden")):
        fixture.repo = repo
        fixture.create_repository()
        files = {
            "main.js": (f"export function {prefix}Target() {{ return 1; }}\n"
                        f"export function {prefix}CallerOne() {{ return {prefix}Target(); }}\n"
                        f"export function {prefix}CallerTwo() {{ return {prefix}Target(); }}\n"
                        "export function sharedAcrossRepos() { return 3; }\n"),
            "main.test.js": (f"import {{ {prefix}Target }} from './main.js';\n"
                             f"export function {prefix}OnlyTest() {{ return {prefix}Target(); }}\n"),
            "duplicate-a.js": "export function duplicateRouteName() { return 4; }\n",
            "Makefile": "all:\n\t@echo fixture\n",
        }
        if prefix == "visible":
            files["duplicate-b.js"] = "export function duplicateRouteName() { return 5; }\n"
        else:
            files["hidden-only.js"] = "export function hiddenPathCanary() { return 6; }\n"
        for name, content in files.items():
            (repo / name).write_text(content)
        for args in (("add", "."), ("-c", "user.name=Fixture", "-c", "user.email=fixture@example.invalid",
                                     "commit", "-m", "authenticated route fixture")):
            command = ["git", "-c", f"core.hooksPath={fixture.git_empty}",
                       "-c", "commit.gpgSign=false", *args]
            result = subprocess.run(command, cwd=repo, env=fixture.env, capture_output=True,
                                    text=True, timeout=30)
            fixture.record(kind="fixture_git", request=command, stdout=result.stdout,
                           stderr=result.stderr, exit_code=result.returncode)
            assert result.returncode == 0, result.stderr
        fixture.run("index", "--repo", repo, "--db", fixture.db)
    fixture.repo = visible
    vault = fixture.root / "vault"
    for name, body in {
        "one/Same.md": "# Same\n\nFirst route note.\n",
        "two/Same.md": "# Same\n\nSecond route note.\n",
        "Unique.md": "# Unique\n\nUnique route note. [[Neighbor]]\n",
        "Neighbor.md": "# Neighbor\n\nLinked route note.\n",
    }.items():
        path = vault / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(body)
    fixture.run("brain", "refresh", vault, "--name", "routes-vault", "--db", fixture.db)
    visible_control = fixture.call("admin", "brain_impact", {"symbol": "visibleTarget", "min_score": 0})
    hidden_control = fixture.call("admin", "brain_impact", {"symbol": "hiddenTarget", "min_score": 0})
    for payload, prefix in ((visible_control, "visible"), (hidden_control, "hidden")):
        assert payload["status"] == "ok" and payload["total"] >= 2, payload
        assert any(row["name"] == prefix + "CallerOne" for row in payload["impact_nodes"]), payload
    fixture.visible_uid = visible_control["target"]
    fixture.hidden_uid = hidden_control["target"]
    repos = json.loads(fixture.run("list-repos", "--db", fixture.db, "--json").stdout)
    assert len(repos) == 2, repos
    hidden_owner = next(repo for repo in repos if repo["url"] == fixture.hidden_repo.as_uri())
    visible_owner = next(repo for repo in repos if repo["url"] == visible.as_uri())
    fixture.visible_repo_uid = visible_owner["uid"]
    fixture.hidden_markers = {
        str(fixture.hidden_repo), fixture.hidden_repo.as_uri(), "hidden_repo",
        hidden_owner["uid"],
        "hiddenTarget", "hiddenCallerOne", "hiddenCallerTwo", "hiddenOnlyTest", "hiddenPathCanary",
        fixture.hidden_uid, *(row["uid"] for row in hidden_control["impact_nodes"]),
    }
    # Learn hidden candidate UIDs from a real admin response, never a store read.
    for name in ("sharedAcrossRepos", "duplicateRouteName"):
        payload = fixture.call("admin", "brain_impact", {"symbol": name, "repo": "hidden_repo"})
        assert payload["status"] == "ok", payload
        fixture.hidden_markers.add(payload["target"])
    fixture.record(kind="release_route_fixture_ready", visible_uid=fixture.visible_uid,
                   hidden_uid=fixture.hidden_uid, daemon_pid=fixture.child.pid)


def auth_cases(fixture):
    for identity in ("missing", "wrong"):
        def reject(who=identity):
            status, _, body = fixture.http(who, "ping")
            assert status == 401 and "error" in body, body
        fixture.case("auth", identity + "-bearer", reject)
    for identity in ("query", "admin"):
        def accept(who=identity):
            status, _, body = fixture.http(who, "ping")
            assert status == 200 and body.get("result") == {}, body
        fixture.case("auth", identity + "-bearer", accept)


def selectors_cases(fixture):
    def ambiguity():
        admin = fixture.call("admin", "brain_impact", {"symbol": "duplicateRouteName"})
        query = fixture.call("query", "brain_impact", {"symbol": "duplicateRouteName"})
        assert admin["status"] == query["status"] == "ambiguous", (admin, query)
        assert len(admin["candidates"]) == 3 and len(query["candidates"]) == 2, (admin, query)
        assert {row["file_path"] for row in query["candidates"]} == {"duplicate-a.js", "duplicate-b.js"}
        fixture.no_hidden(query)
    fixture.case("selectors", "ambiguity-filters-before-candidates", ambiguity)

    def shared():
        admin = fixture.call("admin", "brain_impact", {"symbol": "sharedAcrossRepos"})
        assert admin["status"] == "ambiguous" and len(admin["candidates"]) == 2, admin
        for _ in range(2):
            query = fixture.call("query", "brain_impact", {"symbol": "sharedAcrossRepos"})
            assert query["status"] == "ok", query
            fixture.no_hidden(query)
    fixture.case("selectors", "admin-cache-cannot-supply-hidden-candidate", shared)
    for target in ("hiddenTarget", fixture.hidden_uid, "absentRouteSymbol", "sym:absent-route"):
        def not_found(symbol=target):
            payload = fixture.call("query", "brain_impact", {"symbol": symbol})
            assert payload["status"] == "not_found", payload
            assert payload["impact_nodes"] == [] and payload["total"] == payload["returned"] == 0, payload
            assert not payload.get("candidates"), payload
            # The documented selector echo is caller input, not disclosure.
            assert payload.get("name") == symbol, payload
            assert payload.get("symbol") == symbol, payload
            fixture.no_hidden({key: value for key, value in payload.items()
                               if key not in ("name", "symbol")})
        fixture.case("selectors", "not-found-" + target, not_found)
    def uid_pin():
        payload = fixture.call("query", "brain_impact", {
            "symbol": fixture.visible_uid, "repo": fixture.visible_repo_uid, "limit": 1})
        assert payload["status"] == "ok" and payload["target"] == fixture.visible_uid, payload
        fixture.no_hidden(payload)
    fixture.case("selectors", "visible-uid-with-repo-pin", uid_pin)
    for repo in ("hidden_repo", "absentRouteRepo"):
        def wrong_repo(selector=repo):
            payload = fixture.call("query", "brain_impact", {"symbol": "visibleTarget", "repo": selector})
            assert payload["status"] == "not_found" and payload["impact_nodes"] == [], payload
            assert not payload.get("candidates"), payload
        fixture.case("selectors", "wrong-repo-" + repo, wrong_repo)
    for repo, confidence, score in itertools.product((None, "visible_repo"), (None, 0.1), (None, 0)):
        def capped(repo=repo, confidence=confidence, score=score):
            arguments = {"symbol": "visibleTarget", "limit": 1}
            arguments.update({k: v for k, v in (("repo", repo), ("confidence", confidence), ("min_score", score))
                              if v is not None})
            payload = fixture.call("query", "brain_impact", arguments)
            assert payload["status"] == "ok" and payload["target"] == fixture.visible_uid, payload
            assert payload["total"] >= 2 and payload["returned"] == len(payload["impact_nodes"]) == 1, payload
            assert payload["truncated_by_limit"] is True, payload
            fixture.no_hidden(payload)
        fixture.case("selectors", f"limit-options-{repo}-{confidence}-{score}", capped)
    def threshold():
        payload = fixture.call("query", "brain_impact", {"symbol": "visibleTarget", "min_score": 1})
        assert all(row["impact_score"] >= 1 for row in payload["impact_nodes"]), payload
        assert payload["truncated_by_threshold"] is True, payload
        fixture.no_hidden(payload)
    fixture.case("selectors", "threshold-disclosed", threshold)
    for key, value in (("limit", 0), ("confidence", 1.1), ("min_score", -0.1), ("depth", 16)):
        fixture.case("selectors", "invalid-" + key, lambda k=key, v=value:
                     fixture.call("query", "brain_impact", {"symbol": "visibleTarget", k: v}, error=k))


def gates_cases(fixture):
    def test_selection():
        admin = fixture.call("admin", "affected_tests", {"changed_files": ["main.js"]})
        query = fixture.call("query", "affected_tests", {"changed_files": ["main.js"]})
        assert "hiddenOnlyTest" in json.dumps(admin), admin
        selected = [row for tier in ("tier_1", "tier_2", "tier_3") for row in query[tier]]
        assert any(row["test_file"] == "main.test.js" for row in selected), query
        assert query["recommendation"] == "selection-usable", query
        fixture.no_hidden(query)
    fixture.case("gates", "affected-tests-shared-path-visible-only", test_selection)
    for path in ("missing.js", "Makefile", "hidden-only.js"):
        def conservative(file=path):
            payload = fixture.call("query", "affected_tests", {"changed_files": [file]})
            assert payload["recommendation"] == "run-full-suite", payload
            assert not any(payload[tier] for tier in ("tier_1", "tier_2", "tier_3")), payload
            fixture.no_hidden(payload)
        fixture.case("gates", "unassessed-" + path, conservative)
    def degraded():
        payload = fixture.call("query", "detect_changes", {"changed_files": ["main.js"], "limit": 1})
        assert payload["status"] == "degraded" and payload["gate_state"] == "degraded-unknown", payload
        assert payload["process_analysis_unavailable"] is True and payload["risk"] is None, payload
        assert any(n["descriptor"] == "authz.process-analysis-unavailable" for n in payload["notifications"]), payload
        assert len(payload["affected_symbols"]) == 1 and payload["symbols_omitted"] > 0, payload
        fixture.no_hidden(payload)
    fixture.case("gates", "detect-changes-refuses-false-green-after-cap", degraded)


def unavailable_cases(fixture):
    matrix = {"brain_context": {"seeds": ["visibleTarget"]},
              "code_context": {"seeds": ["visibleTarget"]},
              "cross_repo_contracts": {"name": "visibleTarget"},
              "blast_radius": {"changed_files": ["main.js"]}, "brain_status": {}}
    for tool, arguments in matrix.items():
        def refused(name=tool, args=arguments):
            payload = fixture.call("query", name, args, error="repository-scoped caller")
            fixture.no_hidden(payload)
        fixture.case("unavailable", tool, refused)


def context_cases(fixture):
    control = fixture.call("admin", "brain_context", {"seeds": ["visibleTarget"], "include_seeds": True})
    def no_model():
        assert control["semantic_applied"] is False and control.get("semantic_seed_count", 0) == 0, control
        assert control["seeds_expanded"] >= 1 and control.get("seeds"), control
        assert not any(fixture.models.iterdir()), "no-model fixture unexpectedly acquired model artifacts"
    fixture.case("context", "structural-no-model-positive-control", no_model)
    for seed in ("absent-route-qzv987", "sym:absent", "note:absent", "head:absent",
                 "sec:absent", "tag:absent", "repo:absent", "vlt:absent", "proj:absent"):
        fixture.case("context", "invalid-" + seed, lambda s=seed:
                     fixture.call("admin", "brain_context", {"seeds": [s]}, error="No seeds resolved"))
    for seeds in ([], [""]):
        fixture.case("context", "empty-" + repr(seeds), lambda s=seeds:
                     fixture.call("admin", "brain_context", {"seeds": s}, error="seed"))
    def mixed():
        payload = fixture.call("admin", "brain_context", {
            "seeds": ["visibleTarget", "absent-route-qzv987", "head:absent"], "include_seeds": True})
        assert set(payload["unresolved_seeds"]) == {"absent-route-qzv987", "head:absent"}, payload
        assert payload["semantic_applied"] is False, payload
        for key in ("seeds", "connected"):
            assert {row["uid"] for row in payload.get(key, [])} == {row["uid"] for row in control.get(key, [])}, (control, payload)
    fixture.case("context", "mixed-seeds-only-valid-enrichment", mixed)


def notes_cases(fixture):
    for identity in ("admin", "query"):
        def ambiguous(who=identity):
            payload = fixture.call(who, "note_get", {"title": "Same"})
            assert payload["status"] == "ambiguous", payload
            assert len(set(payload["candidate_uids"])) == 2, payload
            assert {row["file_path"] for row in payload["candidates"]} == {"one/Same.md", "two/Same.md"}, payload
            assert "--repo" not in payload.get("note", ""), payload
        fixture.case("notes", identity + "-ambiguous", ambiguous)
        for path, witness in (("one/Same.md", "First route note"), ("two/Same.md", "Second route note")):
            def found(who=identity, target=path, text=witness):
                payload = fixture.call(who, "note_get", {"title": target})
                assert payload["path"] == target and text in payload["body"], payload
                assert payload["uid"].startswith("note:") and isinstance(payload["outline"], list), payload
            fixture.case("notes", identity + "-path-" + path, found)
        def pinned(who=identity):
            unique = fixture.call(who, "note_get", {"title": "Unique"})
            by_uid = fixture.call(who, "note_get", {"uid": unique["uid"], "include_body": False})
            assert by_uid["uid"] == unique["uid"] and by_uid["path"] == "Unique.md", by_uid
            assert not by_uid.get("body"), by_uid
        fixture.case("notes", identity + "-uid-and-body-opt-out", pinned)
        for arguments in ({"title": "Absent route note"}, {"uid": "note:missing-route"}):
            fixture.case("notes", identity + "-missing-" + str(arguments), lambda who=identity, args=arguments:
                         fixture.call(who, "note_get", args, error="note"))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", required=True)
    parser.add_argument("--timeout", type=int, default=120)
    parser.add_argument("--section", action="append", choices=SECTIONS)
    args = parser.parse_args()
    if not 1 <= args.timeout <= 600:
        parser.error("--timeout must be between 1 and 600 seconds")
    sections = args.section or list(SECTIONS)
    fixture = RouteDaemon(Path(args.binary), args.timeout)
    print(f"Authenticated route evidence: {fixture.root}", flush=True)
    with fixture:
        bootstrap(fixture)
        for section in sections:
            try:
                globals()[section + "_cases"](fixture)
            except Exception as error:
                fixture.failures.append({"section": section, "case": "family-prerequisite", "error": repr(error)})
                fixture.record(kind="release_route_family_failure", section=section, error=repr(error))
        fixture.owned()
        fixture.record(kind="release_authenticated_route_contract", sections=sections,
                       full_matrix=set(sections) == set(SECTIONS), passed=not fixture.failures,
                       failures=fixture.failures, daemon_pid=fixture.child.pid)
        if fixture.failures:
            raise AssertionError(f"{len(fixture.failures)} route cases failed; evidence: {fixture.root}")
    print("release authenticated routes: PASS (" + ", ".join(sections) + ")", flush=True)


if __name__ == "__main__":
    main()
