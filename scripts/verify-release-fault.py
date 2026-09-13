#!/usr/bin/env python3
"""Verify exact-attempt matrix behavior and the checksum-bound staged inventory."""
import argparse
import hashlib
import json
import pathlib
import re

TARGETS = ("aarch64-apple-darwin", "aarch64-unknown-linux-gnu",
           "x86_64-apple-darwin", "x86_64-unknown-linux-gnu")
SMOKE = "Extract and smoke the consumer archive"
FAULT = "Exercise a failed target"
STAGE = "Stage verified target for the all-or-nothing gate"


def require(condition, message):
    if not condition:
        raise ValueError(message)


def verify(run, pages, run_id, attempt, sha, mode, target, bundle, tag):
    require(mode in ("none", "fail", "omit") and target in TARGETS, "invalid fault selection")
    require(re.fullmatch(r"[0-9a-f]{40}", sha) is not None and run_id > 0 and attempt > 0,
            "invalid expected run identity")
    require(run.get("id") == run_id and run.get("run_attempt") == attempt
            and run.get("head_sha") == sha and run.get("event") == "workflow_dispatch"
            and run.get("path", "").split("@")[0] == ".github/workflows/release-please.yml",
            "run identity, attempt, SHA, event or workflow mismatch")
    # The workflow fetches these pages from /runs/ID/attempts/ATTEMPT/jobs,
    # not the latest-attempt endpoint. Each observed job also carries its
    # attempt, so stale retained evidence cannot fill a missing current job.
    require(isinstance(pages, list) and pages, "missing exact-attempt job pages")
    jobs = [job for page in pages for job in page["jobs"]]
    ids = [job["id"] for job in jobs]
    require(len(ids) == len(set(ids)), "duplicate job evidence")
    expected = {f"Build {name}" for name in TARGETS}
    builds = [job for job in jobs if job.get("name", "").startswith("Build ")]
    require(len(builds) == 4 and {job["name"] for job in builds} == expected,
            "missing, duplicate or unexpected matrix jobs")
    for job in builds:
        name = job["name"].removeprefix("Build ")
        require(job.get("run_id") == run_id and job.get("head_sha") == sha
                and job.get("run_attempt") == attempt and job.get("status") == "completed",
                f"{name}: job identity or completion mismatch")
        steps = job.get("steps", [])
        by_name = {}
        for step in steps:
            require(step["name"] not in by_name, f"{name}: duplicate step evidence")
            by_name[step["name"]] = step
        for needed in (SMOKE, FAULT, STAGE):
            require(needed in by_name, f"{name}: missing {needed} evidence")
            require(by_name[needed].get("status") == "completed", f"{name}: incomplete {needed}")
        selected = name == target and mode != "none"
        failed = selected and mode == "fail"
        require(job.get("conclusion") == ("failure" if failed else "success"),
                f"{name}: unexpected matrix job outcome")
        require(by_name[SMOKE].get("conclusion") == "success", f"{name}: consumer smoke did not pass")
        require(by_name[FAULT].get("conclusion") == ("failure" if failed else "skipped"),
                f"{name}: intentional fault was not exercised as selected")
        require(by_name[SMOKE]["number"] < by_name[FAULT]["number"] < by_name[STAGE]["number"],
                f"{name}: fault must follow smoke and precede staging")
        require(by_name[STAGE].get("conclusion") == ("skipped" if selected else "success"),
                f"{name}: wrong target staging outcome")
        require(all(step.get("conclusion") in ("success", "skipped")
                    or (failed and step["name"] == FAULT and step.get("conclusion") == "failure")
                    for step in steps), f"{name}: unrelated step failure/cancellation")
    require(re.fullmatch(r"[A-Za-z0-9._-]+", tag) is not None, "unsafe bundle tag")
    staged = [name for name in TARGETS if mode == "none" or name != target]
    archives = [f"nestweaver-{tag}-{name}.tar.gz" for name in staged]
    expected_files = {name for archive in archives for name in (archive, archive + ".sha256")}
    require(bundle.is_dir(), "missing staged bundle")
    entries = list(bundle.iterdir())
    require({entry.name for entry in entries} == expected_files
            and all(entry.is_file() and not entry.is_symlink() for entry in entries),
            "bundle must contain exactly the selected unaffected targets and checksums")
    for archive in archives:
        checksum = (bundle / (archive + ".sha256")).read_text()
        match = re.fullmatch(r"([0-9a-fA-F]{64}) [ *]" + re.escape(archive) + r"\n?", checksum)
        require(match is not None, f"invalid checksum record for {archive}")
        digest = hashlib.sha256()
        with (bundle / archive).open("rb") as source:
            for chunk in iter(lambda: source.read(1024 * 1024), b""):
                digest.update(chunk)
        require(digest.hexdigest() == match[1].lower(), f"checksum mismatch for {archive}")
    return {"run_id": run_id, "run_attempt": attempt, "head_sha": sha,
            "fault_mode": mode, "fault_target": target, "staged_targets": staged,
            "matrix_jobs": {job["name"]: job["id"] for job in builds}}


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    for option in ("run", "jobs", "sha", "mode", "target", "bundle", "tag"):
        parser.add_argument("--" + option, required=True)
    parser.add_argument("--run-id", type=int, required=True)
    parser.add_argument("--attempt", type=int, required=True)
    args = parser.parse_args()
    try:
        result = verify(json.loads(pathlib.Path(args.run).read_text()),
                        json.loads(pathlib.Path(args.jobs).read_text()), args.run_id,
                        args.attempt, args.sha, args.mode, args.target, pathlib.Path(args.bundle), args.tag)
        print(json.dumps(result, sort_keys=True))
    except (OSError, ValueError, KeyError, TypeError) as error:
        parser.exit(1, f"release matrix proof rejected: {error}\n")
