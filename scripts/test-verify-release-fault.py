#!/usr/bin/env python3
"""Counterexamples to accepting any failing build as intentional fault evidence."""
import copy
import hashlib
import importlib.util
import pathlib
import tempfile
import unittest

spec = importlib.util.spec_from_file_location("verify_fault", pathlib.Path(__file__).with_name("verify-release-fault.py"))
policy = importlib.util.module_from_spec(spec)
spec.loader.exec_module(policy)


class FaultProofTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.bundle = pathlib.Path(self.temp.name)
        self.target = policy.TARGETS[0]
        self.run = {"id": 12, "run_attempt": 2, "head_sha": "a" * 40,
                    "event": "workflow_dispatch", "path": ".github/workflows/release-please.yml"}

    def fixture(self, mode):
        jobs = []
        for index, target in enumerate(policy.TARGETS):
            selected = target == self.target and mode != "none"
            failed = selected and mode == "fail"
            steps = [dict(name=name, status="completed", conclusion=conclusion, number=number)
                     for number, name, conclusion in (
                         (1, "Compile", "success"), (2, policy.SMOKE, "success"),
                         (3, policy.FAULT, "failure" if failed else "skipped"),
                         (4, policy.STAGE, "skipped" if selected else "success"))]
            jobs.append(dict(id=100 + index, name=f"Build {target}", run_id=12, run_attempt=2, head_sha="a" * 40,
                             status="completed", conclusion="failure" if failed else "success", steps=steps))
            if not selected:
                name = f"nestweaver-fixture-{target}.tar.gz"
                data = target.encode()
                (self.bundle / name).write_bytes(data)
                (self.bundle / (name + ".sha256")).write_text(hashlib.sha256(data).hexdigest() + "  " + name + "\n")
        return [{"jobs": jobs}]

    def verify(self, pages, mode):
        return policy.verify(self.run, pages, 12, 2, "a" * 40, mode, self.target, self.bundle, "fixture")

    def test_all_modes_require_their_exact_evidence(self):
        for mode in ("none", "fail", "omit"):
            with self.subTest(mode=mode):
                for path in self.bundle.iterdir():
                    path.unlink()
                self.verify(self.fixture(mode), mode)

    def test_compile_failure_before_fault_is_not_negative_proof(self):
        pages = self.fixture("fail")
        steps = pages[0]["jobs"][0]["steps"]
        steps[0]["conclusion"] = "failure"
        for step in steps[1:]:
            step["conclusion"] = "skipped"
        with self.assertRaisesRegex(ValueError, "consumer smoke"):
            self.verify(pages, "fail")

    def test_an_unaffected_target_failure_is_not_negative_proof(self):
        pages = self.fixture("fail")
        pages[0]["jobs"][1]["conclusion"] = "failure"
        with self.assertRaisesRegex(ValueError, "unexpected matrix job outcome"):
            self.verify(pages, "fail")

    def test_wrong_omitted_target_is_rejected_even_with_three_valid_archives(self):
        pages = self.fixture("omit")
        wrong = policy.TARGETS[1]
        for suffix in (".tar.gz", ".tar.gz.sha256"):
            source = self.bundle / f"nestweaver-fixture-{wrong}{suffix}"
            source.rename(self.bundle / f"nestweaver-fixture-{self.target}{suffix}")
        with self.assertRaisesRegex(ValueError, "exactly the selected"):
            self.verify(pages, "omit")

    def test_stale_duplicate_missing_and_unexpected_job_evidence_refuses(self):
        baseline = self.fixture("fail")
        mutations = [lambda jobs: jobs.pop(), lambda jobs: jobs.append(copy.deepcopy(jobs[0])),
                     lambda jobs: jobs[0].update(head_sha="b" * 40),
                     lambda jobs: jobs[0].update(run_attempt=1),
                     lambda jobs: jobs[0].update(name="Build unsupported-target")]
        for mutate in mutations:
            with self.subTest(mutation=mutate):
                pages = copy.deepcopy(baseline)
                mutate(pages[0]["jobs"])
                with self.assertRaises(ValueError):
                    self.verify(pages, "fail")
        self.run["run_attempt"] = 1
        with self.assertRaisesRegex(ValueError, "run identity"):
            self.verify(baseline, "fail")

    def test_corruption_and_unrelated_failed_post_step_refuse(self):
        pages = self.fixture("fail")
        pages[0]["jobs"][0]["steps"].append(dict(name="Post cleanup", status="completed", conclusion="failure", number=5))
        with self.assertRaisesRegex(ValueError, "unrelated step failure"):
            self.verify(pages, "fail")
        pages[0]["jobs"][0]["steps"].pop()
        archive = next(self.bundle.glob("*.tar.gz"))
        archive.write_bytes(b"corrupted")
        with self.assertRaisesRegex(ValueError, "checksum mismatch"):
            self.verify(pages, "fail")


if __name__ == "__main__":
    unittest.main()
