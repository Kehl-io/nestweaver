#!/usr/bin/env python3
"""Daemon-only F acceptance scaffold against one prebuilt candidate.

Positive lifecycle and ordinary recovery evidence is executable here. Controlled
interruption/late-epilogue failure and stale/corrupt artifact injection require
fixture-only daemon hooks and remain explicitly pending in the evidence report.
This script never builds, opens a graph store, or changes a live sidecar.
"""
import argparse
import json
from pathlib import Path
import subprocess
import time
import urllib.error
import urllib.request

from release_ui import UiDaemon


def git(fixture, root, *args):
    result = subprocess.run(["git", "-c", f"core.hooksPath={fixture.git_empty}",
                             "-c", "commit.gpgSign=false", "-c", "tag.gpgSign=false", *args],
                            cwd=root, env=fixture.env, capture_output=True, text=True,
                            check=True, timeout=30)
    return result.stdout.strip()


def commit(fixture, root, message):
    git(fixture, root, "add", ".")
    git(fixture, root, "-c", "user.name=Fixture", "-c", "user.email=fixture@example.invalid",
        "commit", "--allow-empty", "-m", message)
    return git(fixture, root, "rev-parse", "HEAD")


def owned(fixture):
    assert fixture.child.poll() is None
    assert int(fixture.pidfile.read_text()) == fixture.child.pid


def http(fixture, port, path):
    owned(fixture)
    request = f"http://127.0.0.1:{port}{path}"
    try:
        response = urllib.request.urlopen(request, timeout=10)
    except urllib.error.HTTPError as error:
        response = error
    with response:
        status = response.code
        payload = json.load(response)
        fixture.record(kind="publication_http", request=request, status=status,
                       body=payload, headers=dict(response.headers))
    return status, payload


def wait_suggestions(fixture, port, predicate, timeout=35):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        status, payload = http(fixture, port, "/api/v1/suggest-links")
        if status == 200 and predicate(payload):
            return payload
        if status == 503:
            assert payload["reason"] and "expected_generation" in payload, payload
            assert "force" not in payload.get("message", ""), payload
        else:
            assert status == 200, (status, payload)
        time.sleep(0.25)
    raise AssertionError("suggestions failed to reach the expected current result")


def wait_indexed_symbol(fixture, name):
    deadline = time.monotonic() + 30
    while time.monotonic() < deadline:
        owned(fixture)
        command = [str(fixture.binary), "impact", name, "--db", str(fixture.db), "--json"]
        result = subprocess.run(command, cwd=fixture.root, env=fixture.env,
                                capture_output=True, text=True, timeout=10)
        fixture.record(kind="watcher_symbol_observation", request=command,
                       exit_code=result.returncode, stdout=result.stdout, stderr=result.stderr)
        if result.returncode == 0 and json.loads(result.stdout).get("status") == "ok":
            return
        assert result.returncode in (0, 1, 2), result.stderr
        time.sleep(0.25)
    raise AssertionError("watcher did not publish the changed source symbol")


def malformed_source_recovery(fixture, port, root, dependency):
    """Malformed source must never publish a successful empty manifest map."""
    for name, malformed, restored in [
        ("go.mod", "this is not a Go module\n",
         'module (\n "example.com/app"\n)\nrequire example.com/dep master\n'),
        ("fixture.csproj", "<Project><PackageReference></Project>",
         '<Project Sdk="Microsoft.NET.Sdk"><ItemGroup/></Project>'),
        ("requirements.txt", "requests; python_version >=\n",
         "./downloads/package.whl\nrequests==v2.0; python_version ~= '3.10'\n"),
    ]:
        source = root / name
        source.write_text(malformed)
        deadline = time.monotonic() + 40
        while True:
            status, payload = http(fixture, port, "/api/v1/suggest-links")
            if status == 503 and payload.get("reason") == "source_unavailable":
                assert "links" not in payload and "features" not in payload, payload
                break
            assert status in (200, 503), (status, payload)
            if time.monotonic() >= deadline:
                raise AssertionError(f"{name}: malformed source was not refused: {payload}")
            time.sleep(0.25)
        fixture.record(kind="manifest_malformed_source_refused", source=name, response=payload)
        source.write_text(restored)
        # No explicit index or force-refresh: source restoration is owned by
        # the watcher/coordinator and must recover the full two-repo map.
        current = wait_suggestions(fixture, port, dependency, timeout=70)
        assert current["graph_generation"] >= payload["expected_generation"]
        fixture.record(kind="manifest_source_restoration", source=name, passed=True,
                       response=current)
        source.unlink()
        wait_suggestions(fixture, port, dependency)


def cases(fixture):
    roots, heads = {}, {}
    for name in ("alpha", "beta", "empty"):
        root = fixture.root / name
        root.mkdir()
        git(fixture, root, "init", "--template", str(fixture.git_empty))
        if name != "empty":
            (root / "main.js").write_text(f"export function release{name.title()}Signal() {{ return '{name}'; }}\n")
            dependencies = {"beta-release-package": "1"} if name == "alpha" else {}
            (root / "package.json").write_text(json.dumps({"name": f"{name}-release-package", "dependencies": dependencies}))
        heads[name] = commit(fixture, root, f"{name} initial")
        roots[name] = root
    for name in ("alpha", "beta"):
        fixture.run("index", "--repo", roots[name], "--db", fixture.db)
    # Reuse the signal-safe owner, listener verification, and graceful drain.
    url = fixture.start_ui(extra_args=("--watch", "--repo", str(roots["alpha"])))
    port = int(url.rsplit(":", 1)[1])
    def assert_heads(expected):
        status, repos = http(fixture, port, "/api/v1/repos")
        assert status == 200
        by_root = {str(Path(repo["root_path"]).resolve()): repo for repo in repos}
        for name, sha in expected.items():
            repo = by_root[str(roots[name].resolve())]
            assert repo["indexed_sha"] == sha, (name, sha, repo)
            fixture.record(kind="index_head_witness", case=name, target_head=sha, repo=repo)
        fixture.run("brain", "status", "--db", fixture.db, "--json")

    assert_heads({name: heads[name] for name in ("alpha", "beta")})
    def dependency(payload):
        return any(link["description"] == "Depends on beta-release-package (from manifest)" for link in payload["links"])
    golden = wait_suggestions(fixture, port, dependency)
    malformed_source_recovery(fixture, port, roots["alpha"], dependency)
    fixture.record(kind="release_successful_index_persists_head", phase="fresh-two-repo", passed=True)
    # Watcher edits preserve the recorded completed git revision. A later
    # explicit committed incremental index records its captured target HEAD.
    (roots["alpha"] / "main.js").write_text("export function releaseAlphaChangedSignal() { return 2; }\n")
    wait_indexed_symbol(fixture, "releaseAlphaChangedSignal")
    assert_heads({"alpha": heads["alpha"], "beta": heads["beta"]})
    heads["alpha"] = commit(fixture, roots["alpha"], "alpha incremental")
    fixture.run("index", "--repo", roots["alpha"], "--db", fixture.db)
    assert_heads({"alpha": heads["alpha"], "beta": heads["beta"]})
    fixture.record(kind="release_successful_index_persists_head", phase="incremental-and-watcher", passed=True)
    vault = fixture.root / "vault"
    vault.mkdir()
    note = vault / "Publication.md"
    note.write_text("# Publication\n\nFirst note revision.\n")
    fixture.run("brain", "refresh", vault, "--name", "publication-vault", "--db", fixture.db)
    after_note = wait_suggestions(fixture, port, dependency)
    assert after_note["graph_generation"] > golden["graph_generation"]
    assert after_note["links"] == golden["links"]
    assert after_note["features"] == golden["features"]
    (roots["alpha"] / "package.json").write_text(json.dumps({"name": "alpha-release-package", "dependencies": {}}))
    after_edit = wait_suggestions(fixture, port, lambda result: not dependency(result) and result["graph_generation"] > after_note["graph_generation"])
    assert_heads({"alpha": heads["alpha"], "beta": heads["beta"]})
    fixture.record(kind="release_suggestions_recover_after_generation_change", passed=True,
                   generations=[golden["graph_generation"], after_note["graph_generation"], after_edit["graph_generation"]])
    fixture.run("index", "--repo", roots["empty"], "--db", fixture.db)
    assert_heads({"empty": heads["empty"]})
    fixture.record(kind="release_empty_index_control", passed=True)
    pending = ["stale-cache-startup", "corrupt-and-foreign-artifact-controls",
               "paused-derivation-generation-race", "source-unavailable-rearm", "delete-repository-coverage"]
    fixture.record(kind="release_publication_pending_acceptance", cases=pending, complete=False)
    print("Positive F route cases complete; remaining controlled negatives stay pending.", flush=True)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", required=True)
    args = parser.parse_args()
    fixture = UiDaemon(args.binary)
    print(f"Publication fixture artifacts: {fixture.root}", flush=True)
    with fixture:
        cases(fixture)


if __name__ == "__main__":
    main()
