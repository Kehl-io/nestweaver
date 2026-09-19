"""Pure acceptance-control tests; no NestWeaver executable or database is used."""
from contextlib import redirect_stdout
import errno
import io
from pathlib import Path
import stat
import time
from types import SimpleNamespace
import unittest
from unittest.mock import Mock, patch

import release_publication_failure as publication


class FootprintTests(unittest.TestCase):
    def fixture(self, paths):
        fixture = publication.PublicationFailureDaemon.__new__(
            publication.PublicationFailureDaemon)
        fixture.deadline = time.monotonic() + 30
        fixture.root = Mock()
        fixture.root.rglob.return_value = paths
        fixture.child = None
        return fixture

    def path(self, mode, size):
        path = Mock()
        path.lstat.return_value = SimpleNamespace(st_mode=mode, st_size=size)
        return path

    def check_bounds(self, fixture):
        with patch.object(publication.shutil, "disk_usage",
                          return_value=SimpleNamespace(free=21 * 1024**3)):
            fixture.check_bounds()

    def test_disappearing_temporary_file_does_not_abort_scan_or_hide_remaining_size(self):
        removed = Mock()
        removed.lstat.side_effect = FileNotFoundError(errno.ENOENT, "renamed temporary file")
        regular = self.path(stat.S_IFREG | 0o600, 256 * 1024**2)
        fixture = self.fixture([removed, regular])
        self.check_bounds(fixture)
        removed.lstat.assert_called_once_with()
        regular.lstat.assert_called_once_with()
        # Disappearance must not terminate the scan early and miss later bytes.
        regular.lstat.return_value.st_size += 1
        with self.assertRaisesRegex(RuntimeError, "storage exceeded 256 MiB"):
            self.check_bounds(fixture)

    def test_permission_and_io_errors_are_not_treated_as_disappearance(self):
        for error in (PermissionError(errno.EACCES, "not readable"),
                      OSError(errno.EIO, "metadata read failed")):
            with self.subTest(error=error):
                path = Mock()
                path.lstat.side_effect = error
                with self.assertRaises(type(error)) as caught:
                    self.check_bounds(self.fixture([path]))
                self.assertIs(caught.exception, error)

    def test_symlinks_and_directories_do_not_count_as_regular_file_bytes(self):
        link = self.path(stat.S_IFLNK | 0o777, 300 * 1024**2)
        directory = self.path(stat.S_IFDIR | 0o700, 300 * 1024**2)
        file = self.path(stat.S_IFREG | 0o600, 8)
        self.check_bounds(self.fixture([link, directory, file]))
        for path in (link, directory, file):
            path.lstat.assert_called_once_with()
            path.stat.assert_not_called()


class MainAcceptanceTests(unittest.TestCase):
    class Fixture:
        def __init__(self, cleanup_error=None):
            self.root = Path("unused-fixture-path")
            self.child = SimpleNamespace(returncode=None)
            self.events = []
            self.closed = False
            self.cleanup_error = cleanup_error

        def __enter__(self):
            return self

        def __exit__(self, *_):
            self.closed = True
            if self.cleanup_error:
                raise self.cleanup_error
            self.child.returncode = 0
            self.record(kind="cleanup", exit_code=0)

        def record(self, **event):
            self.events.append(dict(event, after_cleanup=self.closed))

    def invoke(self, fixture, case_error=None):
        output = io.StringIO()

        def observe(actual):
            self.assertIs(actual, fixture)
            if case_error:
                raise case_error
            actual.record(kind="release_content_commit_failure_retry_observed")

        with patch.object(publication, "PublicationFailureDaemon", return_value=fixture), \
                patch.object(publication, "case_one", side_effect=observe), \
                redirect_stdout(output):
            try:
                publication.run_case("unused-binary", publication.case_one,
                                     "release_content_commit_failure_retry")
            except BaseException:
                self.output = output.getvalue()
                raise
        self.output = output.getvalue()

    def test_acceptance_is_emitted_only_after_successful_cleanup(self):
        fixture = self.Fixture()
        self.invoke(fixture)
        self.assertEqual([event["kind"] for event in fixture.events], [
            "release_content_commit_failure_retry_observed", "cleanup",
            "release_content_commit_failure_retry",
        ])
        self.assertNotIn("passed", fixture.events[0])
        final = fixture.events[-1]
        self.assertIs(final["passed"], True)
        self.assertIs(final["after_cleanup"], True)
        self.assertEqual(final["daemon_exit_code"], 0)
        self.assertIn("PASS", self.output)

    def test_cleanup_failure_records_incomplete_without_pass(self):
        for error in (RuntimeError("owned daemon exited with 7"),
                      RuntimeError("owned daemon still draining")):
            with self.subTest(error=error):
                fixture = self.Fixture(cleanup_error=error)
                with self.assertRaises(RuntimeError) as caught:
                    self.invoke(fixture)
                self.assertIs(caught.exception, error)
                self.assertEqual(fixture.events[-1]["kind"], "acceptance_incomplete")
                self.assertFalse(any(event.get("passed") for event in fixture.events))
                self.assertNotIn("PASS", self.output)

    def test_case_failure_still_cleans_up_and_never_becomes_acceptance(self):
        fixture = self.Fixture()
        error = AssertionError("missing expected failure receipt")
        with self.assertRaises(AssertionError) as caught:
            self.invoke(fixture, case_error=error)
        self.assertIs(caught.exception, error)
        self.assertEqual([event["kind"] for event in fixture.events],
                         ["cleanup", "acceptance_incomplete"])
        self.assertFalse(any(event.get("passed") for event in fixture.events))
        self.assertNotIn("PASS", self.output)

    def test_all_four_lifecycle_cases_are_registered(self):
        self.assertEqual([key for key, _, _ in publication.CASES], ["1", "2", "3", "4"])
        self.assertEqual(
            [fn.__name__ for _, _, fn in publication.CASES],
            ["case_one", "case_two", "case_three", "case_four"],
        )


if __name__ == "__main__":
    unittest.main()
