#!/usr/bin/env python3
"""Finite previously fixed dead-code witnesses through one owned daemon.

Uses a prebuilt standard binary, never a direct store or source execution.
The historical corpus measurement remains a separate acceptance step.
"""
import argparse
import json
from isolated_daemon import IsolatedDaemon
from release_dead_code import cli, mcp

SOURCES = {
    "app/Sources/main.swift": "func swiftHelper() {}\nfunc swiftUnused() {}\nclass AppDelegate {}\nswiftHelper()\nAppDelegate()\n",
    "app/Sources/Other.swift": "func nonMainHelper() {}\nnonMainHelper()\n",
    "scripts/tool.swift": "#!/usr/bin/env swift\nfunc shebangHelper() {}\nfunc shebangUnused() {}\nshebangHelper()\n",
    "run.sh": "trap cleanup EXIT\ntrap 'quoted_cleanup' INT TERM\ntrap 'rm -f x' EXIT\ncleanup() { :; }\nquoted_cleanup() { :; }\nunused_shell() { :; }\n",
    "main.rs": "struct Used;\nimpl Used {\n    fn called() { rust_leaf(); }\n}\nstruct Unused;\nimpl Unused {\n    fn uncalled() {}\n}\nfn rust_leaf() {}\nfn main() { Used::called(); }\n",
    # nw-687: a CommonJS `module.exports.X = function` export with no local
    # caller must read LIVE (it might be consumed externally, exactly like an
    # ES export), while a genuinely private, uncalled function in the same
    # file must still read DEAD -- end-to-end proof that
    # is_commonjs_export_assignment/collect_commonjs_reexport_names actually
    # change what release dead-code reports, not just what parse_source sees.
    "index.js": "const h = require('./helpers');\nh.listen();\n",
    "helpers.js": "module.exports.listen = function listen() {\n  return context();\n};\nfunction context() {}\nfunction unused() {}\n",
}
LIVE = {"swiftHelper", "AppDelegate", "shebangHelper", "cleanup", "quoted_cleanup", "rust_leaf", "listen", "context"}
DEAD = {"swiftUnused", "nonMainHelper", "shebangUnused", "unused_shell", "unused"}


def cases(fixture):
    fixture.create_repository()
    for relative, source in SOURCES.items():
        path = fixture.repo / relative
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(source)
    fixture.run("index", "--repo", fixture.repo, "--db", fixture.db)
    # Absence from dead-code is meaningful only after extraction is proven.
    for name in sorted(LIVE | DEAD):
        observed = json.loads(fixture.run("impact", name, "--db", fixture.db, "--json").stdout)
        assert observed["status"] == "ok" and observed["target"], (name, observed)
    local = cli(fixture, limit=1000)
    remote = mcp(fixture, {"repos": ["repo"], "limit": 1000})
    assert local["unreachable_symbols"] == remote["unreachable_symbols"]
    assert local["next_offset"] is None and not local["truncated"], local
    rows = local["unreachable_symbols"]
    names = {row["name"] for row in rows}
    assert not names.intersection(LIVE), rows
    assert DEAD <= names, rows
    assert not any(row["name"] == "Used" and row["kind"] == "Extension" for row in rows), rows
    assert any(row["name"] == "Unused" and row["kind"] == "Extension" for row in rows), rows
    assert not any(row["confidence"] == "high" for row in rows), rows
    fixture.record(kind="release_dead_code_finite_witnesses", passed=True,
                   issues=["nw-489", "nw-490", "nw-491"],
                   live=sorted(LIVE), dead=sorted(DEAD), rows=rows,
                   graph_generation=local["graph_generation"])


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", required=True)
    args = parser.parse_args()
    fixture = IsolatedDaemon(args.binary)
    print(f"Finite dead-code evidence: {fixture.root}", flush=True)
    with fixture:
        cases(fixture)
    print("release_dead_code_finite_witnesses: PASS", flush=True)


if __name__ == "__main__":
    main()
