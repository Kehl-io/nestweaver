#!/usr/bin/env python3
"""Test the registry's stdio launch contract against a disposable indexed repo.

Use --binary for a standard native artifact; optionally pass --launcher for the
packed npm wrapper. Reuses the owned-daemon isolation and retained evidence from
isolated_daemon.py. Never opens a graph directly or changes client settings.
"""
import argparse
import json
from pathlib import Path
import subprocess

from isolated_daemon import IsolatedDaemon


def smoke(fixture, launcher, node=None):
    manifest = json.loads((Path(__file__).resolve().parents[2] / "server.json").read_text())
    arguments = manifest["packages"][0]["packageArguments"]
    assert len(arguments) == 2, arguments
    assert arguments[0]["type"] == "positional" and arguments[0]["value"] == "mcp"
    assert arguments[1]["type"] == "named" and arguments[1]["name"] == "--db"
    command = ([node] if node else []) + [str(launcher), arguments[0]["value"], arguments[1]["name"], str(fixture.db)]
    frames = [
        {"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
            "protocolVersion": "2024-11-05", "capabilities": {},
            "clientInfo": {"name": "registry-smoke", "version": "1"}}},
        {"jsonrpc": "2.0", "method": "notifications/initialized"},
        {"jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {}},
        {"jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": {
            "name": "brain_search", "arguments": {"query": "releaseTarget"}}},
        {"jsonrpc": "2.0", "id": 4, "method": "tools/call", "params": {
            "name": "brain_impact", "arguments": {"symbol": "releaseTarget"}}},
    ]
    result = subprocess.run(command, cwd=fixture.root, env=fixture.env,
                            input="".join(json.dumps(frame) + "\n" for frame in frames),
                            capture_output=True, text=True, timeout=fixture.timeout)
    fixture.record(kind="mcp_registry", request=frames, command=command,
                   stdout=result.stdout, stderr=result.stderr, exit_code=result.returncode)
    assert result.returncode == 0, result.stderr
    replies = {frame["id"]: frame for line in result.stdout.splitlines()
               if "id" in (frame := json.loads(line))}
    for request_id in (1, 2, 3, 4):
        reply = replies[request_id]
        assert "error" not in reply and not reply["result"].get("isError"), reply
    assert replies[1]["result"]["protocolVersion"]
    assert replies[1]["result"]["capabilities"]["tools"] is not None
    tools = {tool["name"] for tool in replies[2]["result"]["tools"]}
    assert {"brain_search", "brain_impact"} <= tools, tools
    search = replies[3]["result"]["structuredContent"]
    assert any(hit["title"] == "releaseTarget" and hit["location"] == "main.js:1"
               for hit in search["results"]), search
    impact = replies[4]["result"]["structuredContent"]
    assert impact["status"] == "ok", impact
    assert any(hit["name"] == "releaseCaller" and hit["edge_type"] == "CALLS"
               for hit in impact["impact_nodes"]), impact
    assert int(fixture.pidfile.read_text().strip()) == fixture.child.pid
    fixture.record(kind="mcp_registry_smoke", passed=True, tool_count=len(tools))
    print(f"MCP initialize, tools/list ({len(tools)}), search, caller impact: PASS", flush=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", required=True)
    parser.add_argument("--launcher", help="Optional packed npm bin/nestweaver executable")
    args = parser.parse_args()
    # Resolve the actual runtime before isolating HOME; version-manager shims
    # otherwise look for their configuration in the disposable home directory.
    node = subprocess.check_output(["node", "-p", "process.execPath"], text=True).strip() if args.launcher else None
    fixture = IsolatedDaemon(args.binary)
    launcher = Path(args.launcher).resolve(strict=True) if args.launcher else fixture.binary
    print(f"Registry fixture artifacts: {fixture.root}", flush=True)
    with fixture:
        fixture.bootstrap()
        smoke(fixture, launcher, node)


if __name__ == "__main__":
    main()
