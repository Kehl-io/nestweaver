#!/usr/bin/env python3
"""Real daemon CLI exit regressions with an unread stderr pipe."""
import argparse
import os
import subprocess
from isolated_daemon import IsolatedDaemon


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--binary', required=True)
    args = parser.parse_args()
    with IsolatedDaemon(args.binary) as fixture:
        fixture.bootstrap()
        for command in [
            ['note', 'get', '__missing_closed_stderr__'],
            ['backlinks', '__missing_closed_stderr__'],
            ['cross-repo-refs', '__missing_closed_stderr__'],
            ['cross-repo-contracts', '__missing_closed_stderr__'],
            ['symbol', '__missing_closed_stderr__'],
            ['flow-trace', '__missing_closed_stderr__'],
            ['project-context', '__missing_closed_stderr__'],
            ['context', '__missing_closed_stderr__'],
        ]:
            argv = [*command, '--db', str(fixture.db)]
            fixture.run(*argv, expected=2)
            reader, writer = os.pipe()
            os.close(reader)
            try:
                result = subprocess.run([str(fixture.binary), *argv], cwd=fixture.root,
                    env=fixture.env, stdout=subprocess.PIPE, stderr=writer, text=True, timeout=60)
            finally:
                os.close(writer)
            fixture.record(kind='closed_stderr', request=argv, exit_code=result.returncode,
                           stdout=result.stdout, expected=2)
            assert result.returncode == 2, (argv, result.returncode, result.stdout)
        # Ambiguous names keep their distinct semantic exit as well.
        for filename in ('first-duplicate.js', 'second-duplicate.js'):
            (fixture.repo / filename).write_text('export function releaseDuplicate() { return 1; }\n')
        fixture.run('index', '--repo', fixture.repo, '--db', fixture.db)
        ambiguous = ['symbol', 'releaseDuplicate', '--db', str(fixture.db)]
        fixture.run(*ambiguous, expected=3)
        reader, writer = os.pipe()
        os.close(reader)
        try:
            result = subprocess.run([str(fixture.binary), *ambiguous], cwd=fixture.root,
                env=fixture.env, stdout=subprocess.PIPE, stderr=writer, text=True, timeout=60)
        finally:
            os.close(writer)
        assert result.returncode == 3, result

        # Preserve successful command output with the same closed stderr.
        reader, writer = os.pipe()
        os.close(reader)
        try:
            result = subprocess.run([str(fixture.binary), 'symbol', 'releaseTarget', '--db', str(fixture.db)],
                cwd=fixture.root, env=fixture.env, stdout=subprocess.PIPE, stderr=writer, text=True, timeout=60)
        finally:
            os.close(writer)
        assert result.returncode == 0 and 'releaseTarget' in result.stdout, result
    return 0


if __name__ == '__main__':
    raise SystemExit(main())
