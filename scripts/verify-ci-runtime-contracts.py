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


class RuntimeContracts(unittest.TestCase):
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

    def test_advisory_and_browser_legacy_builds_enable_feature(self):
        self.assertIn('cargo llvm-cov --workspace --features ci-direct-tests', job(CI, 'coverage'))
        self.assertIn('cargo build --features ci-direct-tests', job(CI, 'e2e'))
        self.assertIn('scripts/ci-direct-cargo.sh', job(CI, 'mutants'))
        self.assertIn('scripts/run-mutation-scope.sh', job(CI, 'mutants'))

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


if __name__ == '__main__':
    unittest.main(verbosity=2)
