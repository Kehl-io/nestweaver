#!/usr/bin/env python3
"""Offline contracts for required runtime lanes and shipping binary isolation.

No Cargo commands, binaries, database processes, or forged CI environment.
"""
from pathlib import Path
import re
import subprocess
import unittest

ROOT = Path(__file__).resolve().parents[1]
CI = (ROOT / '.github/workflows/ci.yml').read_text()
COVERAGE = (ROOT / '.github/workflows/coverage.yml').read_text()
RELEASE = (ROOT / '.github/workflows/release-please.yml').read_text()


def job(source, name):
    match = re.search(r'^  ' + re.escape(name) + r':\n(.*?)(?=^  [a-zA-Z][\w-]*:|\Z)', source, re.M | re.S)
    if match is None:
        raise AssertionError(f'missing job {name}')
    return match[1]


def step(source, name):
    match = re.search(r'^      - name: ' + re.escape(name) + r'\n(.*?)(?=^      - |\Z)', source, re.M | re.S)
    if match is None:
        raise AssertionError(f'missing step {name}')
    return match[1]


def first_cargo_command(section):
    """Offset of the first cargo invocation in a job, ignoring comments."""
    match = re.search(r'^(?!\s*#)[^\n]*?\bcargo (?:build|test|bench|check|clippy|run|llvm-cov|mutants)\b',
                      section, re.M)
    if match is None:
        raise AssertionError('job runs no cargo command')
    return match.start()


class RuntimeContracts(unittest.TestCase):
    def test_coverage_scheduling_is_independent(self):
        self.assertNotRegex(CI, r'(?m)^  coverage:')
        self.assertNotIn('coverage_validation:', CI)
        self.assertIn('name: Coverage\n', COVERAGE)
        self.assertIn('branches: [main]', COVERAGE)
        self.assertIn('workflow_dispatch:', COVERAGE)
        self.assertIn('group: coverage-${{ github.ref }}', COVERAGE)
        self.assertIn('paths: [.github/workflows/coverage.yml]', COVERAGE)
        self.assertNotIn('cancel-in-progress: true', COVERAGE)
        self.assertNotIn('- coverage\n', job(CI, 'required-ci'))
        lane = job(COVERAGE, 'coverage')
        self.assertIn("if: github.event_name == 'workflow_dispatch' || needs.changes.outputs.rust == 'true'", lane)
        self.assertIn('cache-targets: false', lane)
        self.assertIn('NESTWEAVER_NO_DAEMON: "1"', lane)
        self.assertIn('NESTWEAVER_ALLOW_NO_DAEMON: "1"', lane)
        self.assertIn('--no-fail-fast --lcov --output-path lcov.info -- --skip daemon_', lane)
        def rust_paths(source):
            section = source.split('            rust:\n', 1)[1]
            section = re.split(r'^            \w+:|^  \w[\w-]*:', section, flags=re.M)[0]
            return set(re.findall(r"- '([^']+)'", section))
        self.assertEqual(rust_paths(CI), rust_paths(COVERAGE))

    def test_metal_observation_preserves_commands_and_failure_evidence(self):
        metal = job(CI, 'metal-smoke')
        for label in ('metal-cli', 'metal-daemon-contract', 'metal-workspace-units',
                      'metal-daemon-integration-build'):
            self.assertIn('scripts/ci-build-observe.py ' + label + ' -- cargo ', metal)
        self.assertIn('id: metal-cache', metal)
        self.assertIn('steps.metal-cache.outputs.cache-hit', metal)
        self.assertIn('if: always()', step(metal, 'Retain Metal build evidence'))
        self.assertIn('target/cargo-timings', step(metal, 'Retain Metal build evidence'))
        subprocess.run(['python3', str(ROOT / 'scripts/test-ci-build-observe.py')],
                       check=True, capture_output=True, text=True, timeout=30)

    def test_native_dependencies_use_the_acceptance_build_shape(self):
        # A store-only prebuild changes host dependency feature unification
        # (notably cc/parallel) and recompiles the native build-script graph.
        # The acceptance commands already build every dependency they need.
        for name, first_step in (('build-and-check', 'Build CLI'),
                                 ('daemon-tests', 'Daemon-named tests (skipped in the main job)')):
            lane = job(CI, name)
            self.assertIn('cargo metadata --locked --no-deps', lane)
            self.assertNotRegex(lane, r'cargo build[^\n]*-p nestweaver-store')
            self.assertEqual(first_cargo_command(lane),
                             lane.index(step(lane, first_step)) +
                             first_cargo_command(step(lane, first_step)))

    def test_standard_artifact_staged_before_internal_compile(self):
        build = job(CI, 'build-and-check')
        standard = step(build, 'Build CLI')
        self.assertIn('cargo build --locked\n', standard)
        self.assertNotIn('ci-direct-tests', standard)
        self.assertIn('cp target/debug/nestweaver staged/standard/nestweaver', standard)
        self.assertLess(build.index('Stage standard acceptance artifact'), build.index('--features ci-direct-tests'))
        direct = step(build, 'Tests')
        self.assertIn('staged/ci-direct/nestweaver', direct)
        self.assertIn('chmod a-w', direct)

    def test_standard_lane_never_builds_or_uses_internal_artifact(self):
        lane = job(CI, 'standard-daemon')
        self.assertIn('name: ci-standard-linux', lane)
        self.assertNotRegex(lane, r'cargo (build|test)')
        self.assertNotIn('ci-direct-tests', lane)
        self.assertIn('unset NESTWEAVER_NO_DAEMON NESTWEAVER_ALLOW_NO_DAEMON', lane)
        self.assertIn('--binary "$PWD/staged/standard/nestweaver" --ci-policy-test', lane)

    def test_required_reducer_has_both_lanes(self):
        required = job(CI, 'required-ci')
        reducer = (ROOT / 'scripts/verify-required-ci.sh').read_text()
        for name in ('standard-daemon', 'daemon-tests', 'build-and-check'):
            self.assertIn(f'- {name}\n', required)
            self.assertIn(f'require_success {name} || return 1', reducer)

    def test_legacy_workspace_filters_are_complementary(self):
        for name, selector in (('build-and-check', '-- --skip daemon_'), ('daemon-tests', '-- daemon_')):
            lane = job(CI, name)
            self.assertIn('cargo test --locked --workspace --features ci-direct-tests --no-fail-fast ' + selector, lane)
            self.assertIn('NESTWEAVER_ALLOW_NO_DAEMON: "1"', lane)
        manifest = (ROOT / 'Cargo.toml').read_text()
        for name in ('daemon_test', 'parity_test'):
            target = re.search(r'\[\[test\]\]\nname = "' + name + r'"(.*?)(?=\n\[|\Z)', manifest, re.S)[0]
            self.assertIn('required-features = ["ci-direct-tests"]', target)

    def test_metal_standard_acceptance_precedes_internal_tests(self):
        metal = job(CI, 'metal-smoke')
        self.assertLess(metal.index('Standard Metal artifact daemon acceptance'), metal.index('--features metal,ci-direct-tests'))
        self.assertIn('--binary "$PWD/staged/standard/nestweaver" --ci-policy-test', metal)
        for target in ('daemon_test', 'ready_regression_test', 'metal_smoke'):
            self.assertIn('--features metal,ci-direct-tests --test ' + target, metal)
        self.assertIn('NESTWEAVER_ALLOW_NO_DAEMON: "1"', metal)
        self.assertIn('staged/ci-direct/nestweaver', step(metal, 'Populate model cache on CPU'))

    def test_advisory_legacy_builds_enable_feature(self):
        self.assertIn('cargo llvm-cov --workspace --features ci-direct-tests', job(COVERAGE, 'coverage'))
        self.assertIn('scripts/ci-direct-cargo.sh', job(CI, 'mutants'))
        self.assertIn('scripts/run-mutation-scope.sh', job(CI, 'mutants'))

    def test_browser_uses_owned_daemon_and_standard_artifact(self):
        browser = job(CI, 'e2e')
        self.assertIn('cargo build --locked', browser)
        self.assertIn('npm run build', step(browser, 'Build frontend for standard browser acceptance'))
        self.assertLess(browser.index('Build frontend for standard browser acceptance'),
                        browser.index('Build standard daemon and embedded UI'))
        for forbidden in ('ci-direct-tests', 'NESTWEAVER_NO_DAEMON',
                          'NESTWEAVER_ALLOW_NO_DAEMON', '--no-daemon', '/tmp/test.lbug'):
            self.assertNotIn(forbidden, browser)
        self.assertIn('tests/support/release_ui.py', browser)
        self.assertIn('--binary ../../../target/debug/nestweaver', browser)
        config = (ROOT / 'crates/nestweaver-web/frontend/playwright.config.ts').read_text()
        self.assertIn('NESTWEAVER_UI_FIXTURE_URL', config)
        self.assertNotIn('webServer:', config)

    def test_mutation_feature_selection_without_running_cargo_or_forging_ci(self):
        # Exercise only the argument transformer extracted from the workflow.
        # The CI runtime guard and exec are deliberately not executed locally.
        shim = (ROOT / 'scripts/ci-direct-cargo.sh').read_text()
        transform = shim.split('# BEGIN ARGUMENT TRANSFORM', 1)[1].split('\n', 1)[1].split('# END ARGUMENT TRANSFORM', 1)[0]
        transform += 'printf "%s\\0" "${args[@]}"\n'
        cases = [
            (['test', '--package=nestweaver@1.0.0', '--no-run'],
             ['test', '--package=nestweaver@1.0.0', '--no-run', '--features', 'ci-direct-tests']),
            (['test', '-p', 'nestweaver', '--', '--no-fail-fast'],
             ['test', '-p', 'nestweaver', '--features', 'ci-direct-tests', '--', '--no-fail-fast']),
            (['test', '--package=nestweaver-mcp@1.0.0', '--', '--nocapture'],
             ['test', '--package=nestweaver-mcp@1.0.0', '--', '--nocapture']),
            (['metadata', '--format-version', '1'], ['metadata', '--format-version', '1']),
            (['metadata', '-p', 'nestweaver'], ['metadata', '-p', 'nestweaver']),
            (['test', '--', '-p', 'nestweaver'], ['test', '--', '-p', 'nestweaver']),
            (['build', '-pnestweaver@1.0.0'], ['build', '-pnestweaver@1.0.0', '--features', 'ci-direct-tests']),
        ]
        for original, expected in cases:
            with self.subTest(original=original):
                result = subprocess.run(['bash', '-c', transform, 'transform', *original],
                                        check=True, capture_output=True, timeout=5)
                self.assertEqual(result.stdout.decode().split('\0')[:-1], expected)

    def test_shared_mutation_helpers_offline(self):
        for script in ('test-mutation-results.py', 'test-mutation-canary.py'):
            subprocess.run(['python3', str(ROOT / 'scripts' / script)], check=True,
                           capture_output=True, text=True, timeout=20)
        canary = (ROOT / '.github/workflows/mutation-canary.yml').read_text()
        self.assertNotIn('continue-on-error:', canary)
        self.assertNotIn('rust-cache', canary)
        self.assertNotIn('contents: write', canary)
        self.assertIn('timeout-minutes: 8', canary)
        self.assertIn('timeout-minutes: 3', canary)
        self.assertNotIn('- canary-cases', job(CI, 'required-ci'))
        self.assertNotRegex(canary, r'(?m)^\s+(CI|GITHUB_ACTIONS):\s*["\']?(true|1)')

    def test_shipping_features_fail_closed(self):
        build = step(RELEASE, 'Build binary')
        self.assertIn('case "$FEATURES" in', build)
        self.assertIn('""|metal) ;;', build)
        self.assertIn('unsupported shipping features', build)
        self.assertNotIn('--all-features', build)
        self.assertNotIn('ci-direct-tests', build)

    def test_archive_preflight_owns_daemon_and_probes_standard(self):
        smoke = step(RELEASE, 'Extract and smoke the consumer archive')
        self.assertIn('BIN="$EXTRACT_DIR/nestweaver"', smoke)
        self.assertIn('with IsolatedDaemon(sys.argv[1]) as fixture:', smoke)
        self.assertIn('fixture.bootstrap()', smoke)
        self.assertIn('fixture.check_standard_artifact_policy_in_ci()', smoke)
        self.assertIn('--with-trigrams', smoke)
        self.assertIn('fixture.run("search"', smoke)
        self.assertNotIn('NESTWEAVER_ALLOW_NO_DAEMON=1', smoke)
        self.assertNotIn('--no-daemon', smoke)

    def test_ci_binary_has_no_distribution_upload(self):
        for source in (CI, RELEASE):
            for section in re.split(r'^      - ', source, flags=re.M):
                if 'uses: actions/upload-artifact@' in section:
                    self.assertNotIn('staged/ci-direct', section)
        self.assertNotIn('ci-standard-linux', RELEASE)
        self.assertNotIn('staged/ci-direct', RELEASE)

    def test_lbug_source_inputs_trigger_every_compiling_lane(self):
        # A change to the pinned LadybugDB source (or a build entry point that
        # fetches it) must re-run the jobs that compile it, not skip them.
        filters = CI.split('filters: |', 1)[1].split('\n  metal-smoke:', 1)[0]
        rust = filters.split('rust:', 1)[1].split('metal:', 1)[0]
        metal = filters.split('metal:', 1)[1].split('frontend:', 1)[0]
        for path in ("'scripts/fetch-lbug-source.sh'", "'Dockerfile'", "'.devcontainer/**'"):
            self.assertIn(path, rust)
        self.assertIn("'scripts/fetch-lbug-source.sh'", metal)
        for name in ('backlog-performance', 'metal-smoke', 'build-and-check', 'clippy',
                     'daemon-tests', 'mutants', 'e2e'):
            section = job(CI, name)
            self.assertLess(section.index('scripts/fetch-lbug-source.sh'),
                            first_cargo_command(section), name)
        coverage = job(COVERAGE, 'coverage')
        self.assertLess(coverage.index('scripts/fetch-lbug-source.sh'), first_cargo_command(coverage))
        build = job(RELEASE, 'build')
        self.assertLess(build.index('scripts/fetch-lbug-source.sh'), first_cargo_command(build))

    def test_python_only_checks_are_unconditionally_required(self):
        required = job(CI, 'required-ci')
        self.assertIn('if: always()', required)
        for command in ('python3 scripts/test-release-publication-gates.py',
                        'python3 -m unittest discover -s tests/support -p test_isolated_daemon.py -v',
                        'python3 scripts/verify-ci-runtime-contracts.py'):
            self.assertIn(command, required)
        filters = CI.split('filters: |', 1)[1].split('\n  metal-smoke:', 1)[0]
        self.assertIn("'tests/support/isolated_daemon.py'", filters)
        self.assertNotIn("'scripts/test-release-publication-gates.py'", filters)
        self.assertNotIn("'tests/support/test_isolated_daemon.py'", filters)

    def test_no_ci_marker_is_forged(self):
        for source in (CI, RELEASE):
            self.assertNotRegex(source, r'(?m)^\s+(CI|GITHUB_ACTIONS):\s*["\']?(true|1)')

    def test_direct_bypass_requires_github_runner_context(self):
        source = (ROOT / 'src/main.rs').read_text()
        policy = source.split('fn ci_direct_policy(', 1)[1].split('\n}\n', 1)[0]
        for required in ('runner_temp', 'runner_os', 'github_run_id'):
            self.assertIn(required, policy)
        self.assertNotIn('matches!(ci, Some("true" | "1"))', policy)
        allowed = source.split('fn no_daemon_allowed() -> bool {', 1)[1].split('\n}', 1)[0]
        self.assertIn('GITHUB_ACTIONS', allowed)
        self.assertIn('RUNNER_TEMP', allowed)
        self.assertIn('RUNNER_OS', allowed)
        self.assertIn('GITHUB_RUN_ID', allowed)
        self.assertNotIn('var("CI")', allowed)
        golden = (ROOT / 'scripts/golden-check.sh').read_text()
        self.assertIn('unset NESTWEAVER_NO_DAEMON NESTWEAVER_ALLOW_NO_DAEMON', golden)
        self.assertNotIn('NESTWEAVER_NO_DAEMON=1', golden)
        self.assertNotIn('export NESTWEAVER_ALLOW_NO_DAEMON', golden)


if __name__ == '__main__':
    unittest.main(verbosity=2)
