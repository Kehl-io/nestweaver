#!/usr/bin/env python3
"""Real-model daemon acceptance for context seeds and heading-link lifecycle.

Requires an existing Hugging Face model cache; copies it into the disposable
fixture so the test never writes the supplied cache. A missing model, unavailable
vectors, or failed semantic-positive control FAILS this suite, never skips it.
Uses a prebuilt binary and daemon APIs only; never opens a graph store directly.
"""
import argparse
import json
import os
from pathlib import Path
import shutil
import subprocess
import time

from isolated_daemon import IsolatedDaemon


class ContextDaemon(IsolatedDaemon):
    def __init__(self, binary, model_cache, model_id, timeout):
        super().__init__(binary, timeout=timeout)
        source = Path(model_cache).resolve(strict=True)
        if not source.is_dir():
            raise ValueError("--model-cache must be a populated Hugging Face cache directory")
        self.models = self.root / "models"
        self.models.mkdir()
        model_folder = "models--" + model_id.replace("/", "--")
        selected_cache = source / model_folder
        if not selected_cache.is_dir():
            raise ValueError(f"model cache is missing {model_folder}")
        # Copy only the requested model. Do not duplicate unrelated caches or
        # retain links into the user's writable model directory.
        if any(path.is_symlink() and path.is_dir() for path in selected_cache.rglob("*")):
            raise ValueError("model cache directory symlinks are not supported by the bounded fixture")
        model_bytes = sum(path.stat().st_size for path in selected_cache.rglob("*") if path.is_file())
        if model_bytes > 2 * 1024**3 or shutil.disk_usage(self.root).free - model_bytes < 20 * 1024**3:
            raise RuntimeError("model fixture exceeds its 2 GiB budget or 20 GiB free-space floor")
        shutil.copytree(selected_cache, self.models / model_folder)
        self.config = self.root / "instance.toml"
        self.config.write_text(f'''instance_id = "default"
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
model_id = {json.dumps(model_id)}
cache_dir = {json.dumps(str(self.models))}
accelerator = "cpu"
weight_semantic = 0.35
''')
        self.record(kind="context_model_configuration", model_id=model_id,
                    copied_cache=str(self.models), source_cache=str(source))

    def spawn_owned_child(self, command):
        super().spawn_owned_child([*command, "--config", str(self.config)])


def local(payload):
    return payload.get("local_impact", payload)


def cli_context(fixture, seeds, expect_error=False):
    command = [str(fixture.binary), "brain", "context", *seeds, "--json", "--db", str(fixture.db)]
    started = time.monotonic()
    result = subprocess.run(command, cwd=fixture.root, env=fixture.env,
                            capture_output=True, text=True, timeout=fixture.timeout)
    fixture.record(kind="cli_context", request=command, stdout=result.stdout,
                   stderr=result.stderr, exit_code=result.returncode,
                   elapsed_seconds=time.monotonic() - started, daemon_pid=fixture.child.pid)
    if expect_error:
        assert result.returncode == 2, (result.returncode, result.stdout, result.stderr)
        payload = local(json.loads(result.stdout))
        assert payload.get("error") == "not found", payload
        assert payload.get("unresolved_seeds") == seeds, payload
        assert payload.get("seeds_expanded") == 0, payload
        assert not payload.get("connected") and not payload.get("seeds"), payload
        assert payload.get("semantic_seed_count", 0) == 0, payload
        return None
    assert result.returncode == 0, result.stderr
    return local(json.loads(result.stdout))


def mcp(fixture, tool, arguments, expect_error=False):
    frames = [
        {"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
            "protocolVersion": "2024-11-05", "capabilities": {},
            "clientInfo": {"name": "release-context", "version": "1"}}},
        {"jsonrpc": "2.0", "method": "notifications/initialized"},
        {"jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": {
            "name": tool, "arguments": arguments}},
    ]
    command = [str(fixture.binary), "mcp", "--db", str(fixture.db)]
    started = time.monotonic()
    result = subprocess.run(command, cwd=fixture.root, env=fixture.env,
                            input="".join(json.dumps(frame) + "\n" for frame in frames),
                            capture_output=True, text=True, timeout=fixture.timeout)
    fixture.record(kind="mcp_context", request=frames, stdout=result.stdout,
                   stderr=result.stderr, exit_code=result.returncode,
                   elapsed_seconds=time.monotonic() - started, daemon_pid=fixture.child.pid)
    assert result.returncode == 0, result.stderr
    reply = next(frame for line in result.stdout.splitlines()
                 if (frame := json.loads(line)).get("id") == 2)
    if expect_error:
        assert "error" in reply or reply.get("result", {}).get("isError"), reply
        assert "No seeds resolved" in json.dumps(reply), reply
        payload = reply.get("result", {}).get("structuredContent", {})
        assert not payload.get("connected") and not payload.get("seeds"), payload
        assert payload.get("semantic_seed_count", 0) == 0, payload
        return None
    assert "error" not in reply and not reply["result"].get("isError"), reply
    return local(reply["result"]["structuredContent"])


def seed_cases(fixture):
    for route in ("cli", "mcp"):
        def context(seeds, expect_error=False):
            if route == "cli":
                return cli_context(fixture, seeds, expect_error)
            return mcp(fixture, "brain_context", {"seeds": seeds}, expect_error)

        control = context(["Payment"])
        assert control.get("semantic_applied") is True, (
            "Real model and vectors required; an unavailable semantic leg cannot pass", control)
        assert control.get("connected") or control.get("seeds"), control
        fixture.record(kind="release_context_semantic_positive_control", route=route,
                       passed=True, payload=control, daemon_pid=fixture.child.pid)
        for invalid in ["absent-input-qzv987", "sym:absent", "note:absent", "head:absent",
                        "sec:absent", "tag:absent", "repo:absent", "vlt:absent", "proj:absent"]:
            context([invalid], expect_error=True)
            fixture.record(kind="release_context_requires_resolved_seed", route=route,
                           case=invalid, passed=True)
        mixed = context(["Payment", "absent-input-qzv987", "head:absent"])
        assert mixed.get("semantic_applied") is True, mixed
        assert set(mixed["unresolved_seeds"]) == {"absent-input-qzv987", "head:absent"}, mixed
        # Same valid text must drive all enrichment. Compare identities, not
        # floating-point score rendering or mutable latency metadata.
        for key in ("connected", "seeds"):
            assert {item["uid"] for item in mixed.get(key, [])} == {
                item["uid"] for item in control.get(key, [])}, (control, mixed)
        fixture.record(kind="release_context_valid_only_enrichment", route=route,
                       passed=True, payload=mixed)


def anchor_cases(fixture, vault):
    def verify(expected):
        for route in ("cli", "mcp"):
            if route == "cli":
                payload = local(json.loads(fixture.run("brain", "broken-links", "--db", fixture.db,
                                                      "--json", "--limit", "100").stdout))
            else:
                payload = mcp(fixture, "brain_broken_links", {"limit": 100})
            broken = [row for row in payload["broken_links"] if not row.get("resolved_target_uid")]
            targets = [row["wikilink_text"] for row in broken]
            assert sorted(targets) == sorted(expected), payload
            assert payload["unresolved"] == len(expected), payload
            fixture.record(kind="release_missing_anchor_stays_unresolved", route=route,
                           expected=expected, passed=True, payload=payload)
    verify(["Target#Missing", "Absent#Setup"])
    # Keep the source outside the timestamp window to test inbound/unresolved
    # relinking rather than re-indexing every note on each phase.
    for path in vault.glob("*.md"):
        os.utime(path, (1, 1))
    for phase, body, expected in [
        ("delete-heading", "# Target\nbody\n", ["Target#Setup", "Target#Missing", "Absent#Setup"]),
        ("restore-heading", "# Target\n\n## Setup\nrestored\n", ["Target#Missing", "Absent#Setup"]),
        ("rename-heading", "# Target\n\n## Renamed\nrenamed\n", ["Target#Setup", "Target#Missing", "Absent#Setup"]),
        ("restore-both", "# Target\n\n## Setup\nrestored\n\n## Missing\nnow present\n", ["Absent#Setup"]),
    ]:
        (vault / "target.md").write_text(body)
        fixture.run("brain", "refresh", vault, "--name", "context", "--db", fixture.db,
                    "--since", "1970-01-01T00:00:02Z")
        verify(expected)
        fixture.record(kind="release_anchor_lifecycle", phase=phase, passed=True,
                       graph_status=json.loads(fixture.run("brain", "status", "--db", fixture.db, "--json").stdout))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", required=True)
    parser.add_argument("--model-cache", required=True)
    parser.add_argument("--embed-client", help="Daemon-only setup client for an older same-version baseline daemon")
    parser.add_argument("--model-id", default="sentence-transformers/all-MiniLM-L6-v2")
    parser.add_argument("--timeout", type=int, default=300)
    args = parser.parse_args()
    fixture = ContextDaemon(args.binary, args.model_cache, args.model_id, args.timeout)
    print(f"Context evidence: {fixture.root}", flush=True)
    with fixture:
        vault = fixture.root / "vault"
        vault.mkdir()
        for name, body in {
            "payment.md": "# Payment\n\nPayments settle invoices through accounting records. [[Ledger]]\n",
            "ledger.md": "# Ledger\n\nAccounting entries record invoice payment transactions.\n",
            "target.md": "# Target\n\n## Setup\nValid heading.\n",
            "caller.md": "# Caller\n\n[[Target#Setup]] [[Target#Missing]] [[Absent#Setup]]\n",
        }.items():
            (vault / name).write_text(body)
        fixture.run("brain", "add", vault, "--name", "context", "--db", fixture.db)
        original_binary = fixture.binary
        try:
            if args.embed_client:
                old_version = fixture.run("--version").stdout.strip()
                fixture.binary = Path(args.embed_client).resolve(strict=True)
                assert fixture.run("--version").stdout.strip() == old_version, "setup client must not trigger a version restart"
                fixture.record(kind="embedding_setup_client", binary=str(fixture.binary),
                               binary_sha256=fixture.binary_digest(), daemon_binary=str(original_binary))
            fixture.run("embed", "--db", fixture.db, "--scope", "all", "--stats")
        finally:
            fixture.binary = original_binary
        failures = []
        # Retain both baseline defects in one evidence directory: a context
        # assertion failure must not prevent the independent anchor checks.
        for section, run in (("seeds", lambda: seed_cases(fixture)),
                             ("anchors", lambda: anchor_cases(fixture, vault))):
            assert fixture.child.poll() is None
            assert int(fixture.pidfile.read_text().strip()) == fixture.child.pid
            try:
                run()
            except Exception as error:
                failure = {"section": section, "error": repr(error)}
                failures.append(failure)
                fixture.record(kind="release_context_section", passed=False, **failure)
        assert fixture.child.poll() is None
        assert int(fixture.pidfile.read_text().strip()) == fixture.child.pid
        fixture.record(kind="release_context_contract", passed=not failures,
                       failures=failures, daemon_pid=fixture.child.pid)
        if failures:
            raise AssertionError(f"{len(failures)} context/anchor sections failed; evidence: {fixture.root}")
    print("release context and anchors: PASS", flush=True)


if __name__ == "__main__":
    main()
