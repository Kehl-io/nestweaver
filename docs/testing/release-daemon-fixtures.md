# Release daemon fixtures

Local database tests use a real isolated daemon, including fixture indexing.
With a standard binary already built, run:

```sh
python3 tests/support/isolated_daemon.py --binary /absolute/path/to/nestweaver
```

The helper allocates a unique `/tmp/nw-release-*` root, database, HOME/XDG
namespace, and socket identity. Inherited Git routing/configuration variables
are removed; system/global Git configuration, commit signing, templates, and
hooks are disabled for fixture repository creation. It runs `daemon --db <fixture> run` as a child
before indexing through the ordinary CLI. It never launches a build, opens a
store directly, copies a live graph, changes the production daemon, or asks for
bypass. No TCP listener is used by the initial fixture.

Readiness requires the Unix socket, the exact owned child PID in the database's
runtime pidfile, and a successful status query. Evidence includes binary hash,
version, daemon PID, `brain status` graph metadata, commands, responses, exit codes,
and timings in `evidence.jsonl`, plus `daemon.log`. Retain these artifacts with
the release report. Failure preserves the fixture. Cleanup signals only the
unreaped owned child and never escalates past SIGTERM. The child has its own
session/process group and detached stdin, so terminal Ctrl-C cannot reach it.
Parent SIGINT/SIGTERM triggers idempotent cleanup; repeated signals during the
drain are deferred. Successful interrupted cleanup exits with 128 plus the
original signal number. If that child
is still draining after 30 seconds, cleanup reports its PID and preserves all
files. Existing daemon process identities are compared before and after.

After indexing, `brain status --db <fixture> --json` must report one repository
and available daemon runtime telemetry; a degraded direct fallback fails.

Database-free harness safety checks:

```sh
python3 -m unittest discover -s tests/support -p test_isolated_daemon.py -v
```

These tests use only harmless subprocesses and temporary Git repositories.
They cover process-group Ctrl-C, parent SIGTERM, repeated interruption during
cleanup, idempotent cleanup, poisoned Git environment isolation, and status
response validation.

The Rust `release_daemon_test` target wraps this helper. Its
`release_daemon_fixture_bootstrap` case may run locally. The actual
`release_standard_artifact_cannot_bypass` request matrix runs only in real CI
against the standard artifact; local coverage uses pure `no_daemon_gate_tests`.

CI must maintain separate artifacts and required lanes:

- Standard artifact: do not enable `ci-direct-tests`, including via
  `--all-features`; unset `NESTWEAVER_NO_DAEMON` and
  `NESTWEAVER_ALLOW_NO_DAEMON` for daemon coverage. Run `release_daemon_test`.
  The ignored policy case must be explicitly run with
  `--test release_daemon_test release_standard_artifact_cannot_bypass -- --ignored`
  in CI. It adds forbidden requests in its child process only and refuses a
  non-CI runner.
- Internal direct-test artifact: separate target/artifact directory, enable
  `ci-direct-tests`, supply `NESTWEAVER_ALLOW_NO_DAEMON=1`, and a GitHub Actions
  runner context (`GITHUB_ACTIONS=true` plus nonempty `RUNNER_TEMP`,
  `RUNNER_OS`, and `GITHUB_RUN_ID`). Ambient `CI=true` is not a permit.
  The request (`--no-daemon` or `NESTWEAVER_NO_DAEMON`) remains necessary.
  Never publish this artifact. `daemon_test` and `parity_test` are gated here
  because their legacy bootstrap still requests direct stores.

CI-only direct tests do not replace the required daemon lane. Other historical
integration/unit targets must be audited before broad local execution. A test
named “daemon” does not prove that its fixture setup is daemon-only.

The initial fixture covers bootstrap and a simple impact query. Selector
filter combinations, indexing overlap, MCP, restricted identities, and
HTTP/UI routes require additional daemon acceptance cases. The current
frontend Playwright launcher still needs migration from its shared `/tmp` graph
and bypass-requesting local setup before it is safe for local release work.
