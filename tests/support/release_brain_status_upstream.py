#!/usr/bin/env python3
"""Exercise ordinary text status with a private authenticated loopback upstream."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import signal
import sys

from isolated_daemon import IsolatedDaemon

TOKEN = "status-upstream-test-token-0123456789abcdef"


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", required=True)
    parser.add_argument("--expected-sha")
    args = parser.parse_args()
    binary = Path(args.binary).resolve(strict=True)
    digest = hashlib.sha256(binary.read_bytes()).hexdigest()
    if args.expected_sha:
        assert digest == args.expected_sha
    nodes = []
    interrupted = None
    closing = False

    class Node(IsolatedDaemon):
        def __init__(self, server):
            super().__init__(binary, timeout=60)
            nodes.append(self)
            for key in list(self.env):
                if any(word in key.upper() for word in ("TOKEN", "SECRET", "API_KEY", "PASSWORD", "CREDENTIAL", "BYPASS")):
                    self.env.pop(key)
            self.env.update(HF_HUB_OFFLINE="1", TRANSFORMERS_OFFLINE="1")
            authority = self.root / "authority"
            authority.mkdir(mode=0o700)
            self.env["NESTWEAVER_AUTHORITY_DIR"] = str(authority)
            self.config = self.root / "instance.toml"
            quote = lambda value: json.dumps(str(value))
            self.config.write_text(
                f'instance_id = "status-regression"\ndb = {quote(self.db)}\n'
                f'[snapshot_storage]\nbackend = "local"\npath = {quote(self.root / "snapshots")}\n'
                f'[workspace]\nbackend = "local"\npath = {quote(self.root / "workspace")}\n'
                '[inference]\nendpoint = "http://127.0.0.1:9"\nembedding_model = "unused"\nsummary_model = "unused"\n'
                '[git]\ncredential_method = "ssh"\n[embedding]\nauto_repair_cache = false\n[watch]\nenabled = false\n'
            )
            self.extra = ["--config", str(self.config)]
            if server:
                self.env["NESTWEAVER_AUTH_TOKEN"] = TOKEN
                self.extra += ["--server", "--bind", "127.0.0.1:0", "--port-file", str(self.root / "ports")]

        def spawn_owned_child(self, command):
            super().spawn_owned_child([*command, *self.extra])
            if interrupted is not None and not closing:
                raise SystemExit(128 + interrupted)

    def handle_signal(signum, _frame):
        nonlocal interrupted
        interrupted = interrupted or signum
        if not closing and not any(node._spawning for node in nodes):
            raise SystemExit(128 + interrupted)

    previous = {sig: signal.signal(sig, handle_signal) for sig in (signal.SIGINT, signal.SIGTERM)}
    report = {"binary_sha256": digest, "scope": "ordinary CLI text status and private authenticated user-registry upstream", "outcome": "FAIL"}
    try:
        peer = Node(server=True)
        client = Node(server=False)
        peer.start()
        client.start()
        client.bootstrap()
        grpc_port = int((peer.root / "ports").read_text().splitlines()[0])
        # Match dirs::config_dir on each platform; never use the real registry.
        config_home = Path(client.env["HOME"]) / "Library/Application Support" if sys.platform == "darwin" else Path(client.env["XDG_CONFIG_HOME"])
        registry = config_home / "nestweaver/upstreams.toml"
        registry.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
        registry.write_text(f'[[upstream]]\nname = "status-peer"\nurl = "grpc://127.0.0.1:{grpc_port}"\ntoken = "{TOKEN}"\nmode = "fallback"\ntimeout = "2s"\n')
        registry.chmod(0o600)
        registry_before = registry.read_bytes()
        # Deliberately text, with normal user-registry discovery. nw-930's
        # authenticated per-source vault inventory exercises reactor ownership
        # without claiming separate HealthCheck/RepoStates observations.
        status = client.run("brain", "status", "--db", client.db, timeout=30)
        assert "Vault source: status-peer (available, fallback mode)\n  Vaults: 0" in status.stdout, status.stdout
        assert TOKEN not in status.stdout + status.stderr
        assert f"grpc://127.0.0.1:{grpc_port}" not in status.stdout
        assert "there is no reactor running" not in status.stderr
        assert registry.read_bytes() == registry_before
        assert int(client.pidfile.read_text()) == client.child.pid
        assert int(peer.pidfile.read_text()) == peer.child.pid
        report.update(outcome="PASS", text_upstream_summary=True, authenticated_vault_inventory=True, registry_unchanged=True)
    except BaseException as error:
        report.update(error_type=type(error).__name__, error=str(error), no_reactor_failure="there is no reactor running" in str(error))
    finally:
        closing = True
        cleanup = []
        for node in reversed(nodes):
            try:
                node.close()
                cleanup.append({"root": str(node.root), "exit": node.child.returncode if node.child else None})
            except BaseException as error:
                report["outcome"] = "FAIL"
                cleanup.append({"root": str(node.root), "error_type": type(error).__name__})
        report["cleanup"] = cleanup
        report["roots"] = [str(node.root) for node in nodes]
        if interrupted is not None:
            report.update(outcome="FAIL", interruption_signal=interrupted)
        if report["outcome"] == "PASS":
            for node in nodes:
                shutil.rmtree(node.root)
            report["private_roots_removed"] = True
        else:
            for node in nodes:
                (node.root / "status-regression-result.json").write_text(json.dumps(report, indent=2) + "\n")
        for sig, handler in previous.items():
            signal.signal(sig, handler)
        print(json.dumps(report), flush=True)
    return 0 if report["outcome"] == "PASS" else 1


if __name__ == "__main__":
    raise SystemExit(main())
