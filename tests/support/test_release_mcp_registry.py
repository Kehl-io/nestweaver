#!/usr/bin/env python3
"""Owned foreground subprocess regressions; no database or native build needed."""
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

from release_mcp_registry import run_owned_stdio


@unittest.skipUnless(os.name == 'posix', 'POSIX registry smoke ownership')
class OwnedStdioTests(unittest.TestCase):
    def test_normal_native_exit_preserves_stdio(self):
        result = run_owned_stdio([sys.executable, '-c', 'import sys; print(sys.stdin.read()); print("err", file=sys.stderr)'],
                                  cwd=None, env=os.environ.copy(), input='input', timeout=3)
        self.assertEqual(result.returncode, 0)
        self.assertIn('input', result.stdout)
        self.assertEqual(result.stderr, 'err\n')

    def test_timeout_terminates_wrapper_and_native_descendant_and_retains_failure(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            child = root / 'native.py'
            stopped = root / 'stopped'
            child.write_text('import os,signal,sys,time\n'
                             'def stop(*args):\n'
                             ' open(sys.argv[1],"w").write(str(os.getpid()))\n'
                             ' raise SystemExit(0)\n'
                             'signal.signal(signal.SIGTERM,stop)\n'
                             'print("native-ready",flush=True)\n'
                             'while True: time.sleep(.1)\n')
            wrapper = root / 'wrapper.py'
            wrapper.write_text('import subprocess,sys,time\n'
                               'p=subprocess.Popen([sys.executable,sys.argv[1],sys.argv[2]])\n'
                               'print("wrapper-ready",flush=True)\n'
                               'while True: time.sleep(.1)\n')
            # A protected sibling proves cleanup does not signal a parent or
            # unrelated process group. It has its own Popen ownership handle.
            protected = subprocess.Popen([sys.executable, '-c', 'import time; time.sleep(30)'])
            try:
                with self.assertRaises(subprocess.TimeoutExpired) as failure:
                    run_owned_stdio([sys.executable, str(wrapper), str(child), str(stopped)],
                                    cwd=root, env=os.environ.copy(), input='', timeout=.5)
                self.assertIn('wrapper-ready', failure.exception.stdout)
                self.assertIn('native-ready', failure.exception.stdout)
                self.assertTrue(stopped.is_file(), 'native child must receive owned group termination')
                self.assertIsNone(protected.poll(), 'protected sibling must be unchanged')
            finally:
                protected.terminate()
                protected.wait(timeout=3)

    def test_node_wrapper_early_exit_still_cleans_foreground_child(self):
        node = subprocess.check_output(['node', '-p', 'process.execPath'], text=True).strip()
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            stopped = root / 'stopped'
            child = root / 'native.py'
            child.write_text('import signal,sys,time\n'
                             'def stop(*args):\n'
                             ' open(sys.argv[1],"w").write("stopped")\n'
                             ' raise SystemExit(0)\n'
                             'signal.signal(signal.SIGTERM,stop)\n'
                             'print("native-ready",flush=True)\n'
                             'while True: time.sleep(.1)\n')
            wrapper = root / 'wrapper.cjs'
            wrapper.write_text('require("child_process").spawn(process.argv[2],process.argv.slice(3),{stdio:"inherit"});\n'
                               'setTimeout(()=>process.exit(0),100);\n')
            with self.assertRaises(subprocess.TimeoutExpired):
                run_owned_stdio([node, str(wrapper), sys.executable, str(child), str(stopped)],
                                cwd=root, env=os.environ.copy(), input='', timeout=.5)
            self.assertTrue(stopped.is_file())

    def test_uncooperative_native_is_killed_after_bounded_grace(self):
        with self.assertRaises(subprocess.TimeoutExpired) as failure:
            run_owned_stdio([sys.executable, '-c',
                'import signal,time; signal.signal(signal.SIGTERM,signal.SIG_IGN); print("ready",flush=True); time.sleep(30)'],
                cwd=None, env=os.environ.copy(), input='', timeout=.3, grace=.2)
        self.assertIn('ready', failure.exception.stdout)

    def test_interruption_cleans_the_owned_process_and_is_not_suppressed(self):
        original = subprocess.Popen

        class InterruptOnce(original):
            def communicate(self, *args, **kwargs):
                if not getattr(self, '_interrupted_once', False):
                    self._interrupted_once = True
                    self.stdout.readline()
                    raise KeyboardInterrupt()
                return super().communicate(*args, **kwargs)

        with patch('release_mcp_registry.subprocess.Popen', InterruptOnce):
            with self.assertRaises(KeyboardInterrupt):
                run_owned_stdio([sys.executable, '-c', 'import time; print("ready",flush=True); time.sleep(30)'],
                                cwd=None, env=os.environ.copy(), input='', timeout=3, grace=.2)


if __name__ == '__main__':
    unittest.main()
