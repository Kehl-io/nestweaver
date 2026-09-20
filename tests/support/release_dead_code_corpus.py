#!/usr/bin/env python3
"""Fixed-source parser population comparison through sequential owned daemons.

Requires two prebuilt standard artifacts. Writes a manual audit worksheet;
never infers precision, opens a store, builds, clones or force-kills a writer.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path, PurePosixPath
import shutil
import signal
import subprocess
import tarfile
import time

from isolated_daemon import IsolatedDaemon


CORPUS_COMMIT = "809b0e1fa3185c9f4e9b8a825430086c54075328"
CORPUS_TREE = "98d930d1b7ab2d5d680b79be85002591ff15df78"
GIB = 1024 ** 3
SOURCE_LIMIT = 256 * 1024 ** 2
MAX_OUTPUT = 32 * 1024 ** 2


def sha256(path):
    digest = hashlib.sha256()
    with Path(path).open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def save(fixture, name, payload):
    path = fixture.root / name
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(payload, indent=2) + "\n")
    fixture.record(kind="corpus_artifact", path=str(path), sha256=sha256(path))
    return path


class CorpusDaemon(IsolatedDaemon):
    def __init__(self, old_binary, candidate):
        super().__init__(old_binary, timeout=120)
        self.candidate = Path(candidate).resolve(strict=True)
        self.repo = self.root / "nestweaver-audit-corpus"
        self.env["NESTWEAVER_INDEX_TIMEOUT_SECS"] = "600"
        self.env["NESTWEAVER_RPC_TIMEOUT_SECS"] = "600"
        self.stage = "old"
        self.active_client = None
        self.record(kind="corpus_binaries", old=str(self.binary), old_sha256=self.binary_digest(),
                    candidate=str(self.candidate), candidate_sha256=sha256(self.candidate))

    def owned(self):
        assert self.child is not None and self.child.poll() is None, "owned daemon exited"
        assert self.pidfile.exists() and int(self.pidfile.read_text()) == self.child.pid, "daemon ownership changed"

    def resources(self):
        free = shutil.disk_usage(self.root).free
        size = 0
        for directory, _, files in os.walk(self.root, followlinks=False):
            for name in files:
                try:
                    size += (Path(directory) / name).lstat().st_size
                except FileNotFoundError:
                    pass  # daemon may atomically replace a sidecar between entries
                if size > 2 * GIB:
                    raise RuntimeError("owned fixture exceeded 2 GiB; stop and preserve evidence")
        assert free >= 20 * GIB, "less than 20 GiB free; stop and preserve evidence"
        return {"fixture_bytes": size, "free_bytes": free}

    def run(self, *args, **kwargs):
        if self.child is not None:
            self.owned()
        result = super().run(*args, **kwargs)
        if self.child is not None:
            self.owned()
        return result

    def close(self):
        try:
            super().close()
        finally:
            log = self.root / "daemon.log"
            if log.exists():
                self.record(kind="corpus_daemon_log", stage=self.stage, path=str(log),
                            sha256=sha256(log), complete=self.child is not None and self.child.poll() is not None)

    def stop_stage(self):
        self.owned()
        # Protect the whole stop/reap/log transfer from a second signal. The
        # base signal handler remembers it; no replacement starts afterwards.
        self._closing = True
        try:
            self._close_owned_child()
            assert self.child.poll() is not None, "old owner did not exit"
            self.record(kind="corpus_stage_stopped", stage=self.stage, pid=self.child.pid,
                        exit_code=self.child.returncode)
            assert self.child.returncode == 0, "old owner did not exit cleanly"
            log = self.root / "daemon.log"
            archived = self.root / ("daemon-" + self.stage + ".log")
            log.rename(archived)
            self.record(kind="corpus_daemon_log", stage=self.stage, path=str(archived), sha256=sha256(archived))
            self.child = None
            self.log = None
        finally:
            self._closing = False
        if self._interrupted is not None:
            raise SystemExit(128 + self._interrupted)

    def start_candidate(self):
        assert self.child is None, "candidate cannot overlap the old owner"
        self.resources()
        self.binary = self.candidate
        self.stage = "candidate"
        self.start()
        self.owned()
        self.record(kind="corpus_candidate_started", pid=self.child.pid, binary_sha256=self.binary_digest())

    def bounded_index(self, force=False):
        self.owned()
        self.record(kind="corpus_index_start", stage=self.stage, **self.resources())
        command = [str(self.binary), "index", "--repo", str(self.repo), "--db", str(self.db), "--json"]
        if force:
            command.append("--force")
        stem = "index-" + self.stage
        stdout_path, stderr_path = self.root / (stem + ".json"), self.root / (stem + ".stderr")
        started = time.monotonic()
        try:
            with stdout_path.open("wb") as out, stderr_path.open("wb") as err:
                self._spawning = True
                try:
                    self.active_client = subprocess.Popen(command, cwd=self.root, env=self.env,
                                                          stdin=subprocess.DEVNULL, stdout=out, stderr=err)
                finally:
                    self._spawning = False
                if self._interrupted is not None:
                    raise SystemExit(128 + self._interrupted)
                while self.active_client.poll() is None:
                    self.owned()
                    self.resources()
                    assert time.monotonic() - started < 600, "ten-minute index client deadline exceeded"
                    assert max(stdout_path.stat().st_size, stderr_path.stat().st_size) <= MAX_OUTPUT, "index output bound exceeded"
                    try:
                        self.active_client.wait(timeout=1)
                    except subprocess.TimeoutExpired:
                        pass
                assert self.active_client.returncode == 0, "index failed; inspect retained stage output"
            self.owned()
            self.record(kind="corpus_index_result", stage=self.stage, request=command,
                        elapsed_seconds=time.monotonic() - started, daemon_pid=self.child.pid,
                        stdout_sha256=sha256(stdout_path), stderr_sha256=sha256(stderr_path), **self.resources())
            payload = json.loads(stdout_path.read_text())
            save(self, stem + "-parsed.json", payload)
            return payload
        finally:
            # Client termination is not writer cancellation. __exit__ still
            # enters the owned daemon's ordinary SIGTERM drain on any failure.
            self._closing = True
            try:
                if self.active_client is not None and self.active_client.poll() is None:
                    self.active_client.send_signal(signal.SIGTERM)
                    try:
                        self.active_client.wait(timeout=30)
                    except subprocess.TimeoutExpired:
                        self.record(kind="corpus_client_still_running", pid=self.active_client.pid,
                                    stage=self.stage, no_escalation=True)
                self.active_client = None
            finally:
                self._closing = False
            if self._interrupted is not None:
                raise SystemExit(128 + self._interrupted)


def git(fixture, repo, *args):
    command = ["git", "-c", f"core.hooksPath={fixture.git_empty}", "-c", "commit.gpgSign=false",
               "-c", "tag.gpgSign=false", *map(str, args)]
    result = subprocess.run(command, cwd=repo, env=fixture.env, stdin=subprocess.DEVNULL,
                            capture_output=True, text=True, timeout=60)
    fixture.record(kind="corpus_git", request=command, cwd=str(repo), stdout=result.stdout,
                   stderr=result.stderr, exit_code=result.returncode)
    assert result.returncode == 0, result.stderr
    return result.stdout.strip()


def verify_source(fixture):
    assert git(fixture, fixture.repo, "rev-parse", "HEAD^{tree}") == CORPUS_TREE
    assert not git(fixture, fixture.repo, "status", "--porcelain", "--untracked-files=all"), "corpus changed"


def export_corpus(fixture, source_repo):
    fixture.resources()
    source_repo = Path(source_repo).resolve(strict=True)
    assert git(fixture, source_repo, "rev-parse", CORPUS_COMMIT + "^{tree}") == CORPUS_TREE
    archive = fixture.root / "corpus.tar"
    git(fixture, source_repo, "archive", "--format=tar", "--output", archive, CORPUS_COMMIT)
    assert archive.stat().st_size <= SOURCE_LIMIT, "archive exceeds 256 MiB"
    fixture.repo.mkdir()
    with tarfile.open(archive, "r:") as bundle:
        members = bundle.getmembers()
        assert len(members) <= 10000 and sum(member.size for member in members) <= SOURCE_LIMIT
        for member in members:
            path = PurePosixPath(member.name)
            assert not path.is_absolute() and ".." not in path.parts and ".git" not in path.parts
            assert member.isdir() or member.isfile(), "links/special files are not accepted in corpus archive"
            destination = fixture.repo.joinpath(*path.parts)
            if member.isdir():
                destination.mkdir(parents=True, exist_ok=True)
                continue
            destination.parent.mkdir(parents=True, exist_ok=True)
            with bundle.extractfile(member) as source, destination.open("wb") as output:
                shutil.copyfileobj(source, output, length=1024 * 1024)
            destination.chmod(0o755 if member.mode & 0o111 else 0o644)
    git(fixture, fixture.repo, "init", "--template", fixture.git_empty)
    git(fixture, fixture.repo, "add", "--force", ".")
    assert git(fixture, fixture.repo, "write-tree") == CORPUS_TREE, "export eligibility changed the source tree"
    git(fixture, fixture.repo, "-c", "user.name=Fixture", "-c", "user.email=fixture@example.invalid",
        "commit", "-m", "fixed historical corpus")
    verify_source(fixture)
    fixture.record(kind="corpus_source", commit=CORPUS_COMMIT, tree=CORPUS_TREE,
                   fixture_commit=git(fixture, fixture.repo, "rev-parse", "HEAD"),
                   archive_sha256=sha256(archive), archive_bytes=archive.stat().st_size,
                   tracked_files=len([member for member in members if member.isfile()]))


def collect_pages(fixture, stage, scope):
    rows, first, token, generation, offset = [], None, None, None, 0
    for number in range(200):
        fixture.owned()
        fixture.resources()
        arguments = ["dead-code", "--db", fixture.db, "--repo", scope,
                     "--min-confidence", "low", "--limit", "1000", "--offset", str(offset), "--json"]
        if number:
            arguments += ["--generation", str(generation), "--page-token", token]
        payload = json.loads(fixture.run(*arguments).stdout)
        save(fixture, f"{stage}/page-{number:03d}.json", payload)
        assert not payload.get("refused"), payload
        assert payload["review_only"] is True and payload["high_confidence_available"] is False
        assert payload["paging_scope"] == "local_database" and payload["offset"] == offset
        if first is None:
            first, token, generation = payload, payload["page_token"], payload["graph_generation"]
        assert payload["page_token"] == token and payload["graph_generation"] == generation
        for field in ("total_symbols", "reachable_symbols", "unreachable_count", "matching_count", "excluded_count"):
            assert payload[field] == first[field], (field, first[field], payload[field])
        batch = payload["unreachable_symbols"]
        assert payload["returned"] == len(batch) <= 1000
        assert all(row["confidence"] in ("low", "medium") for row in batch)
        rows.extend(batch)
        if payload["next_offset"] is None:
            assert not payload["has_more"]
            break
        assert batch and payload["has_more"] and payload["next_offset"] == offset + len(batch)
        offset = payload["next_offset"]
    else:
        raise AssertionError("200-page export bound exceeded; full set is incomplete")
    uids = [row["uid"] for row in rows]
    assert len(uids) == len(set(uids)) == first["matching_count"] == first["unreachable_count"]
    save(fixture, stage + "/complete.json", {"summary": {k: v for k, v in first.items() if k != "unreachable_symbols"},
                                             "unreachable_symbols": rows})
    return rows, first


def audit_packet(fixture, stage, rows):
    packet = []
    for ordinal, row in enumerate(rows[:15], 1):
        detail = json.loads(fixture.run("symbol", row["uid"], "--db", fixture.db, "--json").stdout)
        symbol = detail["symbol"]
        assert symbol["uid"] == row["uid"]
        relative = PurePosixPath(symbol["file_path"])
        assert not relative.is_absolute() and ".." not in relative.parts
        source = fixture.repo.joinpath(*relative.parts)
        assert source.is_file() and source.stat().st_size <= SOURCE_LIMIT
        lines = source.read_text(errors="replace").splitlines()
        start, end = max(1, symbol["start_line"] - 4), min(len(lines), symbol["end_line"] + 4)
        excerpt_end = min(end, start + 159)
        packet.append({"ordinal": ordinal, "candidate": row, "detail": detail,
                       "source_sha256": sha256(source), "excerpt_start": start, "excerpt_end": excerpt_end,
                       "excerpt_truncated": excerpt_end < end,
                       "source_excerpt": "\n".join(f"{n}: {lines[n-1]}" for n in range(start, excerpt_end + 1)),
                       "verdict": None, "source_justification": None, "reference_evidence": []})
    save(fixture, stage + "/manual-audit.json", {"review_required": True, "precision": None, "rows": packet})


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--old-binary", required=True)
    parser.add_argument("--binary", required=True)
    parser.add_argument("--source-repo", required=True)
    args = parser.parse_args()
    fixture = CorpusDaemon(args.old_binary, args.binary)
    print(f"Fixed-corpus evidence: {fixture.root}", flush=True)
    with fixture:
        export_corpus(fixture, args.source_repo)
        fixture.bounded_index()
        verify_source(fixture)
        scope = fixture.repo.name
        repos = json.loads(fixture.run("list-repos", "--db", fixture.db, "--json").stdout)
        assert len(repos) == 1, "legacy unscoped output requires exactly one fixture repo"
        save(fixture, "old/repos.json", repos)
        # The old standard CLI predates --repo and pagination. This fixture has
        # exactly one repo, so the legacy first-15 population is unambiguous.
        legacy = json.loads(fixture.run("dead-code", "--db", fixture.db,
                                        "--min-confidence", "low", "--limit", "15", "--json").stdout)
        save(fixture, "old/top15.json", legacy)
        audit_packet(fixture, "old", legacy["unreachable_symbols"])
        save(fixture, "old/status.json", json.loads(fixture.run("brain", "status", "--db", fixture.db, "--json").stdout))
        fixture.stop_stage()
        fixture.start_candidate()
        before, before_summary = collect_pages(fixture, "before-reindex", scope)
        audit_packet(fixture, "before-reindex", before)
        verify_source(fixture)
        fixture.bounded_index(force=True)
        verify_source(fixture)
        after, after_summary = collect_pages(fixture, "after-reindex", scope)
        audit_packet(fixture, "after-reindex", after)
        old_rows, new_rows = {row["uid"]: row for row in before}, {row["uid"]: row for row in after}
        save(fixture, "population-delta.json", {
            "comparison": "new classifier over old parse versus new classifier over forced new parse",
            "source_commit": CORPUS_COMMIT, "source_tree": CORPUS_TREE,
            "before_summary": {k: v for k, v in before_summary.items() if k != "unreachable_symbols"},
            "after_summary": {k: v for k, v in after_summary.items() if k != "unreachable_symbols"},
            "removed": [old_rows[uid] for uid in sorted(old_rows.keys() - new_rows.keys())],
            "added": [new_rows[uid] for uid in sorted(new_rows.keys() - old_rows.keys())],
            "changed": [{"before": old_rows[uid], "after": new_rows[uid]}
                        for uid in sorted(old_rows.keys() & new_rows.keys()) if old_rows[uid] != new_rows[uid]],
            "manual_precision_review_pending": True})
        fixture.stop_stage()
        fixture.record(kind="release_fixed_corpus_export", passed=True, precision_measured=False, **fixture.resources())
    print("Fixed-corpus export complete; manual precision review remains required.", flush=True)


if __name__ == "__main__":
    main()
