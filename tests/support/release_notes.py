#!/usr/bin/env python3
"""Note outcome acceptance through an owned daemon, using a prebuilt binary."""
import argparse
import json

from isolated_daemon import IsolatedDaemon
from release_selectors import mcp


def cases(fixture):
    vault = fixture.root / "vault"
    for folder, body in (("one", "First fixture body"), ("two", "Second fixture body")):
        path = vault / folder / "Same.md"
        path.parent.mkdir(parents=True)
        path.write_text(f"# Same\n\n{body}\n\n## Detail\n\nSection body.\n")
    (vault / "Unique.md").write_text("# Unique\n\nUnique fixture body.\n")
    fixture.run("brain", "refresh", vault, "--name", "fixture-vault", "--db", fixture.db)
    failures = []

    def check(name, operation):
        if fixture.child.poll() is not None or int(fixture.pidfile.read_text()) != fixture.child.pid:
            raise RuntimeError("fixture daemon ownership lost")
        try:
            operation()
            fixture.record(kind="release_note_case", case=name, passed=True)
        except Exception as error:
            failures.append({"case": name, "error": repr(error)})
            fixture.record(kind="release_note_case", case=name, passed=False, error=repr(error))

    def cli(target, expected=0, text=False):
        flags = [] if text else ["--json"]
        result = fixture.run("note", "get", target, "--db", fixture.db, *flags, expected=expected)
        return result if text else json.loads(result.stdout)

    def ambiguous(payload):
        assert payload["status"] == "ambiguous", payload
        assert len(payload["candidate_uids"]) == 2, payload
        assert len(set(payload["candidate_uids"])) == 2, payload
        assert {c["file_path"] for c in payload["candidates"]} == {"one/Same.md", "two/Same.md"}, payload
        assert "--repo" not in payload.get("note", ""), payload

    def found(payload, path, witness):
        assert payload["uid"].startswith("note:"), payload
        assert payload["path"] == path, payload
        assert witness in payload["body"], payload
        assert isinstance(payload["outline"], list), payload

    check("cli-ambiguous-json", lambda: ambiguous(cli("Same", expected=3)))
    check("mcp-ambiguous", lambda: ambiguous(mcp(fixture, "note_get", {"title": "Same"})))

    def ambiguity_text():
        result = cli("Same", expected=3, text=True)
        assert "notes" in result.stderr and "symbols" not in result.stderr, result.stderr
        assert "--repo" not in result.stderr, result.stderr
        assert "note:" in result.stderr, result.stderr
    check("cli-ambiguous-text", ambiguity_text)

    for path, witness in (("one/Same.md", "First fixture body"),
                          ("two/Same.md", "Second fixture body"),
                          ("Unique.md", "Unique fixture body")):
        check(f"cli-path-{path}", lambda p=path, w=witness: found(cli(p), p, w))
        check(f"mcp-path-{path}", lambda p=path, w=witness: found(mcp(fixture, "note_get", {"title": p}), p, w))

    def uid_pin():
        payload = cli("Unique")
        found(payload, "Unique.md", "Unique fixture body")
        uid = payload["uid"]
        found(cli(uid), "Unique.md", "Unique fixture body")
        found(mcp(fixture, "note_get", {"uid": uid}), "Unique.md", "Unique fixture body")
    check("unique-title-and-uid", uid_pin)

    def missing():
        for target in ("Absent fixture note", "note:missing"):
            payload = cli(target, expected=2)
            assert payload["error"] == "not found", payload
    check("cli-missing-title-and-uid", missing)

    def body_control():
        payload = mcp(fixture, "note_get", {"title": "Unique", "include_body": False})
        assert payload["uid"] and not payload.get("body"), payload
    check("mcp-body-opt-out", body_control)
    fixture.record(kind="release_note_outcome_survives_daemon", passed=not failures, failures=failures)
    if failures:
        raise AssertionError(f"{len(failures)} note cases failed; evidence: {fixture.root}")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", required=True)
    args = parser.parse_args()
    fixture = IsolatedDaemon(args.binary)
    print(f"Note evidence: {fixture.root}", flush=True)
    with fixture:
        cases(fixture)
    print("release note outcomes: PASS", flush=True)


if __name__ == "__main__":
    main()
