#!/usr/bin/env python3
"""Offline contracts for required runtime lanes and shipping binary isolation.

No Cargo commands, binaries, database processes, or forged CI environment.
"""
from pathlib import Path
import hashlib
import json
import os
import re
import shutil
import subprocess
import sys
import tempfile
import textwrap
import unittest

ROOT = Path(__file__).resolve().parents[1]
CI = (ROOT / '.github/workflows/ci.yml').read_text()
COVERAGE = (ROOT / '.github/workflows/coverage.yml').read_text()
MUTATION = (ROOT / '.github/workflows/mutation.yml').read_text()
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
    offsets = [match.start()] if match else []
    helper = 'python3 scripts/prewarm-mutation-build.py'
    if helper in section:
        offsets.append(section.index(helper))
    if not offsets:
        raise AssertionError('job runs no cargo command')
    return min(offsets)


class MutationScope(unittest.TestCase):
    """Execute only scope selection in disposable Git repos, never Cargo."""

    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        self.root = Path(self.directory.name)
        self.env = {key: value for key, value in os.environ.items() if not key.startswith('GIT_')}
        self.env.update(GIT_CONFIG_NOSYSTEM='1', GIT_CONFIG_GLOBAL=os.devnull)
        empty = self.root / 'empty'; empty.mkdir()
        self.git('init', '--initial-branch=main', '--template', str(empty))
        scripts = self.root / 'scripts'; scripts.mkdir()
        shutil.copyfile(ROOT / 'scripts/select-mutation-packages.py', scripts / 'select-mutation-packages.py')
        self.write('src/lib.rs', 'pub fn root() -> bool { false }\n')
        self.write('crates/nestweaver-parser/src/lib.rs', 'pub fn parse() -> bool { false }\n')
        self.base = self.commit('base')
        self.write('src/lib.rs', 'pub fn root() -> bool { true }\n')
        self.write('crates/nestweaver-parser/src/lib.rs', 'pub fn parse() -> bool { true }\n')
        self.head = self.commit('candidate')
        # Main advances independently: a two-dot diff would wrongly include its
        # unrelated package. Both selection artifacts must use the merge base.
        self.git('checkout', '-b', 'advanced-base', self.base)
        self.write('crates/foreign/src/lib.rs', 'pub fn unrelated() {}\n')
        self.advanced = self.commit('main advanced')
        self.git('update-ref', 'refs/remotes/origin/main', self.advanced)
        self.git('checkout', '--detach', self.head)

    def git(self, *args):
        return subprocess.check_output(['git', '-c', 'commit.gpgSign=false',
                                        '-c', 'core.hooksPath=' + str(self.root / 'empty'), *args],
                                       cwd=self.root, env=self.env, text=True,
                                       stderr=subprocess.DEVNULL).strip()

    def write(self, relative, value):
        path = self.root / relative; path.parent.mkdir(parents=True, exist_ok=True); path.write_text(value)

    def commit(self, message):
        self.git('add', '.')
        self.git('-c', 'user.name=Fixture', '-c', 'user.email=fixture@example.invalid',
                 'commit', '-m', message)
        return self.git('rev-parse', 'HEAD')

    def select(self, base='main', expected=None):
        source = MUTATION.split('# BEGIN MUTATION SCOPE\n', 1)[1].split('# END MUTATION SCOPE', 1)[0]
        self.scope = self.root / 'selected'
        environment = dict(self.env, MUTATION_BASE_REF=base, EXPECTED_SHA=expected or self.head,
                           MUTATION_SCOPE_DIR=str(self.scope), GITHUB_OUTPUT=str(self.root / 'output'))
        return subprocess.run([sys.executable, '-c', textwrap.dedent(source)], cwd=self.root,
                              env=environment, capture_output=True, text=True, timeout=10)

    def test_branch_diff_and_packages_share_one_resolved_range(self):
        result = self.select()
        self.assertEqual(result.returncode, 0, result.stderr)
        record = json.loads((self.scope / 'selection.json').read_text())
        self.assertEqual(record['base_sha'], self.advanced)
        self.assertEqual(record['merge_base'], self.base)
        self.assertEqual(record['candidate_sha'], self.head)
        self.assertEqual(record['range'], self.base + '..' + self.head)
        self.assertEqual((self.scope / 'files.txt').read_text().splitlines(),
                         ['crates/nestweaver-parser/src/lib.rs', 'src/lib.rs'])
        self.assertEqual((self.scope / 'packages.txt').read_text().splitlines(),
                         ['nestweaver', 'nestweaver-parser'])
        self.assertNotIn('foreign', (self.scope / 'input.diff').read_text())
        for name, digest in record['sha256'].items():
            self.assertEqual(hashlib.sha256((self.scope / name).read_bytes()).hexdigest(), digest)

    def test_explicit_commit_and_empty_selection_are_legitimate(self):
        result = self.select(self.base)
        self.assertEqual(result.returncode, 0, result.stderr)
        record = json.loads((self.scope / 'selection.json').read_text())
        self.assertEqual(record['base_sha'], self.base)
        result = self.select(self.head)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual((self.scope / 'files.txt').read_bytes(), b'')
        self.assertEqual((self.scope / 'packages.txt').read_bytes(), b'')

    def test_invalid_or_missing_base_and_wrong_candidate_refuse(self):
        for base in ('missing-branch', '--help', 'bad branch', '$(touch SHOULD_NOT_EXIST)', 'f' * 40):
            with self.subTest(base=base):
                result = self.select(base)
                self.assertNotEqual(result.returncode, 0)
        self.assertFalse((self.root / 'SHOULD_NOT_EXIST').exists())
        result = self.select(expected=self.base)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('checkout does not match', result.stderr)


class RuntimeContracts(unittest.TestCase):
    def test_canary_helper_changes_supply_runtime_prerequisites(self):
        rust = CI.split('            rust:\n', 1)[1].split('            metal:', 1)[0]
        for script in ('mutation-canary.py', 'verify-mutation-prerequisite.py'):
            self.assertIn("'scripts/" + script + "'", rust)
        self.assertIn('python3 scripts/test-mutation-canary.py', job(CI, 'required-ci'))

    def test_coverage_scheduling_is_independent(self):
        self.assertNotRegex(CI, r'(?m)^  coverage:')
        self.assertNotIn('coverage_validation:', CI)
        self.assertIn('name: Coverage\n', COVERAGE)
        trigger = COVERAGE.split('on:\n', 1)[1].split('\npermissions:', 1)[0]
        self.assertNotRegex(trigger, r'(?m)^  (push|pull_request):')
        self.assertIn('schedule:\n', trigger)
        self.assertIn("cron: '17 4 * * 0'", trigger)
        self.assertIn('workflow_dispatch:', COVERAGE)
        self.assertIn('group: coverage-${{ github.ref }}', COVERAGE)
        self.assertNotIn('cancel-in-progress: true', COVERAGE)
        self.assertNotIn('- coverage\n', job(CI, 'required-ci'))
        lane = job(COVERAGE, 'coverage')
        self.assertIn("if: github.event_name == 'workflow_dispatch' || github.event_name == 'schedule'", lane)
        self.assertNotRegex(COVERAGE, r'(?m)^  validate:')
        rust = CI.split('            rust:\n', 1)[1].split('            metal:', 1)[0]
        self.assertNotIn("'.github/workflows/coverage.yml'", rust)
        required = job(CI, 'required-ci')
        self.assertIn('if: always()', required)
        self.assertIn('python3 scripts/verify-ci-runtime-contracts.py', required)
        self.assertIn('"$tool_dir/actionlint" .github/workflows/*.yml', required)
        self.assertIn('cache-targets: false', lane)
        self.assertIn('NESTWEAVER_NO_DAEMON: "1"', lane)
        self.assertIn('NESTWEAVER_ALLOW_NO_DAEMON: "1"', lane)
        self.assertIn('--no-fail-fast --lcov --output-path lcov.info -- --skip daemon_', lane)

    def test_mutation_is_opt_in_without_weakening_its_verdict(self):
        self.assertNotRegex(CI, r'(?m)^  mutants:')
        trigger = MUTATION.split('on:\n', 1)[1].split('\npermissions:', 1)[0]
        self.assertIn('workflow_dispatch:', trigger)
        self.assertNotRegex(trigger, r'(?m)^  (push|pull_request|schedule):')
        self.assertIn('base_ref:', trigger)
        self.assertIn('ci_run_id:', trigger)
        mutations = job(MUTATION, 'mutants')
        self.assertNotIn('continue-on-error:', mutations)
        self.assertNotIn('github.base_ref', mutations)
        self.assertIn('fetch-depth: 0', mutations)
        self.assertIn('ref: ${{ github.sha }}', mutations)
        self.assertIn('save-if: false', mutations)
        self.assertIn('if: always()', step(mutations, 'Surviving mutants (read these — each one is a test that cannot fail)'))
        self.assertIn('helper.observe_selected(', mutations)
        self.assertLess(mutations.index('Verify successful exact-candidate CI prerequisite'),
                        first_cargo_command(mutations))

    def test_linux_jobs_share_the_same_cache_profile(self):
        for name in ('build-and-check', 'daemon-tests', 'clippy', 'e2e'):
            lane = job(CI, name)
            self.assertIn('CARGO_PROFILE_DEV_DEBUG: "0"', lane, name)
            self.assertIn('CARGO_PROFILE_TEST_DEBUG: "0"', lane, name)

    def test_linux_shared_cache_has_one_successful_main_writer(self):
        for name in ('build-and-check', 'daemon-tests', 'clippy', 'e2e'):
            lane = job(CI, name)
            caches = re.findall(r'      - uses: Swatinem/rust-cache@[0-9a-f]{40}[^\n]*\n(.*?)(?=^      - |\Z)',
                                lane, re.M | re.S)
            self.assertEqual(len(caches), 1, name)
            cache = caches[0]
            self.assertIn('shared-key: build', cache, name)
            if name == 'build-and-check':
                self.assertIn("save-if: ${{ github.ref == 'refs/heads/main' }}", cache)
                self.assertIn('cache-on-failure: false', cache)
                self.assertNotIn('cache-on-failure: true', cache)
            else:
                self.assertIn('save-if: false', cache, name)
                self.assertNotIn("github.ref == 'refs/heads/main'", cache, name)

    def test_metal_observation_preserves_commands_and_failure_evidence(self):
        metal = job(CI, 'metal-smoke')
        for label in ('metal-cli', 'metal-daemon-contract', 'metal-workspace-units',
                      'metal-daemon-integration-build'):
            self.assertIn('scripts/ci-build-observe.py ' + label + ' ', metal)
        self.assertIn('id: metal-cache', metal)
        self.assertIn('steps.metal-cache.outputs.cache-hit', metal)
        self.assertIn('if: always()', step(metal, 'Retain Metal build evidence'))
        self.assertIn('target/cargo-timings', step(metal, 'Retain Metal build evidence'))
        subprocess.run(['python3', str(ROOT / 'scripts/test-ci-build-observe.py')],
                       check=True, capture_output=True, text=True, timeout=30)

    def test_metal_contract_and_units_share_features_and_execute_the_named_test(self):
        metal = job(CI, 'metal-smoke')
        contract = step(metal, 'Verify daemon cache-only contract')
        units = step(metal, 'Workspace unit tests (macOS portability gate)')
        self.assertIn('--workspace --lib --features metal,ci-direct-tests', contract)
        self.assertIn('--features metal,ci-direct-tests --workspace --lib', units)
        self.assertNotIn('-p nestweaver-daemon', contract)
        name = 'server::embedding_load_config_tests::daemon_embedding_startup_constructs_cached_model_offline'
        self.assertIn('--require-test ' + name, contract)
        self.assertIn('-- ' + name + ' --exact', contract)
        self.assertLess(metal.index('Standard Metal artifact daemon acceptance'),
                        metal.index('--workspace --lib --features metal,ci-direct-tests'))

    def test_metal_cache_cannot_publish_failed_or_cancelled_builds(self):
        metal = job(CI, 'metal-smoke')
        cache = re.search(r'        id: metal-cache\n(.*?)(?=^      - |\Z)',
                          metal, re.M | re.S)
        self.assertIsNotNone(cache)
        self.assertIn('cache-on-failure: false', cache[1])
        self.assertNotIn('cache-on-failure: true', cache[1])
        # Isolate from immutable incomplete caches saved under the old key.
        self.assertIn('shared-key: metal-smoke-complete-v1', cache[1])
        self.assertNotRegex(cache[1], r'shared-key: metal-smoke\s*$')

    def test_mutation_prewarm_and_tests_use_the_same_package_union(self):
        mutations = job(MUTATION, 'mutants')
        self.assertIn('cargo-nextest@0.9.140', mutations)
        self.assertIn('scripts/prewarm-mutation-build.py "$MUTATION_SCOPE_DIR/packages.txt"', mutations)
        self.assertNotIn('for pkg in', step(mutations, 'Warm the -p build shape before the mutation budget starts'))
        helper = (ROOT / 'scripts/run-mutation-scope.sh').read_text()
        self.assertIn('test_pkg_args+=(--test-package "$pkg")', helper)
        self.assertIn('"${pkg_args[@]}" "${test_pkg_args[@]}"', helper)
        self.assertIn('CARGO_INCREMENTAL=1 timeout', helper)
        self.assertIn('--test-tool nextest --cargo-test-arg=--fail-fast', helper)
        self.assertNotIn('--no-fail-fast', helper)
        self.assertIn('--build-timeout 300', helper)
        self.assertIn('--budget "$MUTANTS_BUDGET"', mutations)
        shim = (ROOT / 'scripts/ci-direct-cargo.sh').read_text()
        self.assertIn('ci-nextest-with-docs.py', shim)
        subprocess.run(['python3', str(ROOT / 'scripts/test-mutation-build-reuse.py')],
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
        self.assertIn('scripts/ci-direct-cargo.sh', job(MUTATION, 'mutants'))
        self.assertIn('scripts/run-mutation-scope.sh', job(MUTATION, 'mutants'))

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
            (['nextest', 'run', '--package=nestweaver@1.0.0', '--no-run'],
             ['nextest', 'run', '--package=nestweaver@1.0.0', '--no-run', '--features', 'ci-direct-tests']),
            (['nextest', 'run', '-p', 'nestweaver-store', '--fail-fast'],
             ['nextest', 'run', '-p', 'nestweaver-store', '--fail-fast']),
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
                     'daemon-tests', 'e2e'):
            section = job(CI, name)
            self.assertLess(section.index('scripts/fetch-lbug-source.sh'),
                            first_cargo_command(section), name)
        coverage = job(COVERAGE, 'coverage')
        self.assertLess(coverage.index('scripts/fetch-lbug-source.sh'), first_cargo_command(coverage))
        mutations = job(MUTATION, 'mutants')
        self.assertLess(mutations.index('scripts/fetch-lbug-source.sh'), first_cargo_command(mutations))
        build = job(RELEASE, 'build')
        self.assertLess(build.index('scripts/fetch-lbug-source.sh'), first_cargo_command(build))

    def test_python_only_checks_are_unconditionally_required(self):
        required = job(CI, 'required-ci')
        self.assertIn('if: always()', required)
        for command in ('python3 scripts/test-release-publication-gates.py',
                        'python3 scripts/test-release-lockfile-publication.py',
                        'python3 -m unittest discover -s tests/support -p test_isolated_daemon.py -v',
                        'python3 scripts/verify-ci-runtime-contracts.py'):
            self.assertIn(command, required)
        filters = CI.split('filters: |', 1)[1].split('\n  metal-smoke:', 1)[0]
        self.assertIn("'tests/support/isolated_daemon.py'", filters)
        self.assertNotIn("'scripts/test-release-publication-gates.py'", filters)
        self.assertNotIn("'scripts/test-release-lockfile-publication.py'", filters)
        self.assertNotIn("'tests/support/test_isolated_daemon.py'", filters)

    def test_no_ci_marker_is_forged(self):
        for source in (CI, RELEASE, MUTATION):
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
