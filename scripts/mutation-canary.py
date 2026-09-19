#!/usr/bin/env python3
"""Run three tiny real-Cargo infrastructure canaries only inside real GitHub CI."""
import argparse
import difflib
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import time

ROOT = Path(__file__).resolve().parents[1]
SCRIPTS = ROOT / 'scripts'
ROOT_PACKAGE = 'nestweaver'
SIBLING = 'nestweaver-canary-sibling'


def load(name):
    spec = importlib.util.spec_from_file_location(name, SCRIPTS / (name + '.py'))
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def write_json(path, value):
    path.write_text(json.dumps(value, indent=2) + '\n')


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def check_counts(case, result):
    if case == 'comment':
        expected = dict(planned=0, accounted=0, caught=0, survived=0, timed_out=0, unviable=0,
                        outstanding=0, status=0, exit_status=0, outer_exhausted=False, complete=False)
        if 'legitimate-zero' not in result['reason']:
            raise ValueError('zero result was not explicitly recognized')
    elif case == 'complete':
        expected = dict(planned=2, accounted=2, caught=2, survived=0, timed_out=0, unviable=0,
                        outstanding=0, status=0, exit_status=0, outer_exhausted=False, complete=True)
    else:
        count = result['timed_out']
        if type(count) is not int or count not in (0, 1) or result['status'] not in (124, 137):
            raise ValueError('negative case lacks actual timeout evidence')
        if 'outer budget exhausted' not in result['reason']:
            raise ValueError('negative case failed for an unrelated reason')
        expected = dict(planned=1, accounted=count, caught=0, survived=0, timed_out=count, unviable=0,
                        outstanding=1-count, exit_status=1, outer_exhausted=True, complete=False)
    if any(type(result.get(key)) is not type(value) or result[key] != value for key, value in expected.items()):
        raise ValueError(f'{case} count contract failed: {result}')


def source(root):
    feature = 'assert!(cfg!(feature = "ci-direct-tests"));' if root else ''
    return '''// canary comment after
pub fn ready() -> bool {
    true // body after
}
#[cfg(test)]
mod tests {
    #[test]
    fn observes_ready() {
        FEATURE
        let value = super::ready();
        if std::env::var_os("MUTATION_CANARY_HOLD").is_some() && !value {
            let marker = std::env::var("MUTATION_CANARY_MARKER").unwrap();
            let digest = std::env::var("MUTATION_CANARY_DIGEST").unwrap();
            let epoch = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs_f64();
            std::fs::write(marker, format!("{} {} {}", std::process::id(), digest, epoch)).unwrap();
            std::thread::sleep(std::time::Duration::from_secs(60));
        }
        assert!(value);
    }
}
'''.replace('FEATURE', feature)


def run(command, cwd, env, log, check=True):
    started = time.monotonic_ns()
    with log.open('w') as stream:
        result = subprocess.run(command, cwd=cwd, env=env, stdout=stream, stderr=subprocess.STDOUT)
    if check and result.returncode != 0:
        raise ValueError(f'command failed ({result.returncode}); see {log.name}')
    return dict(argv=[str(a) for a in command], status=result.returncode,
                started_ns=started, finished_ns=time.monotonic_ns())


def trace_records(path):
    return [json.loads(line) for line in path.read_text().splitlines()]


def selected_package(argv):
    for n, arg in enumerate(argv):
        if arg == '--':
            break
        if arg in ('-p', '--package'):
            return argv[n+1].split('@')[0]
        if arg.startswith('--package='):
            return arg.split('=', 1)[1].split('@')[0]
    return None


def check_trace(records, result):
    # Join start-only exec traces to authentic successful/failed phase records.
    phases = set()
    for verdict in result['verdicts']:
        for phase in verdict['phase_results'] or []:
            if phase['phase'] not in ('Build', 'Test'):
                continue
            argv = phase['argv'][1:]
            package = selected_package(argv)
            matches = [r for r in records if r['original_argv'] == argv]
            if not matches:
                raise ValueError('mutation phase did not use the shared wrapper')
            for record in matches:
                effective = record['effective_argv']
                before_separator = effective[:effective.index('--')] if '--' in effective else effective
                injected = 'ci-direct-tests' in before_separator
                if injected != (package == ROOT_PACKAGE) or record['injected_feature'] != injected:
                    raise ValueError('incorrect package feature interception')
            phases.add((package, phase['phase']))
    if phases != {(p, phase) for p in (ROOT_PACKAGE, SIBLING) for phase in ('Build', 'Test')}:
        raise ValueError('missing root/sibling actual mutation build/test interception')


def check_identity(plan, expected):
    report = load('report-mutation-results')
    if report.plan_ids(plan) != report.plan_ids(expected):
        raise ValueError('canary plan did not match exact discovered identities')


def github_runner_context(environment):
    """Check expected runner context; environment values cannot prove origin."""
    return environment.get('GITHUB_ACTIONS') == 'true' and all(
        environment.get(key) for key in ('RUNNER_TEMP', 'RUNNER_OS', 'GITHUB_RUN_ID'))


def run_cases(evidence):
    if not github_runner_context(os.environ):
        raise ValueError('canary execution requires GitHub Actions runner context')
    if os.environ.get('MUTATION_CANARY_HOLD'):
        raise ValueError('negative setting may only be enabled for the negative case')
    fixture = Path(os.environ['RUNNER_TEMP']) / 'mutation-fixture'
    fixture.mkdir()  # Refuse stale directories, rather than mixing evidence.
    paths = ['Cargo.toml', 'src/lib.rs', f'crates/{SIBLING}/Cargo.toml', f'crates/{SIBLING}/src/lib.rs']
    for path in paths:
        (fixture / path).parent.mkdir(parents=True, exist_ok=True)
    (fixture / paths[0]).write_text('[package]\nname="nestweaver"\nversion="0.0.0"\nedition="2021"\n[features]\nci-direct-tests=[]\n[workspace]\nmembers=["crates/nestweaver-canary-sibling"]\nresolver="2"\n')
    (fixture / paths[2]).write_text('[package]\nname="nestweaver-canary-sibling"\nversion="0.0.0"\nedition="2021"\n')
    (fixture / paths[1]).write_text(source(True))
    (fixture / paths[3]).write_text(source(False))
    env = dict(os.environ, GIT_CONFIG_NOSYSTEM='1', GIT_CONFIG_GLOBAL='/dev/null',
               CARGO_TARGET_DIR=str(fixture / 'target'), CARGO_NET_OFFLINE='true')
    # This fixture has no native dependencies and does not inherit repo flags/config.
    env.pop('RUSTFLAGS', None)
    env.pop('NESTWEAVER_NO_DAEMON', None)
    env.pop('CARGO_ENCODED_RUSTFLAGS', None)
    real_cargo = subprocess.check_output(['rustup', 'which', 'cargo'], text=True).strip()
    shim = Path(os.environ['RUNNER_TEMP']) / 'mutation-cargo'
    shim.mkdir()
    (shim / 'cargo').symlink_to(SCRIPTS / 'ci-direct-cargo.sh')
    env.update(NW_REAL_CARGO=real_cargo, CARGO=str(shim / 'cargo'), PATH=str(shim) + os.pathsep + env['PATH'],
               NESTWEAVER_ALLOW_NO_DAEMON='1')
    versions = {name: subprocess.check_output([exe, '--version'], text=True).strip()
                for name, exe in [('cargo', real_cargo), ('rustc', 'rustc'), ('cargo-mutants', 'cargo-mutants')]}
    if versions['cargo-mutants'] != 'cargo-mutants 27.1.0':
        raise ValueError('unexpected mutation tool version')
    write_json(evidence / 'versions.json', versions)
    executables = {'cargo': real_cargo,
                   'rustc': subprocess.check_output(['rustup', 'which', 'rustc'], text=True).strip(),
                   'cargo-mutants': shutil.which('cargo-mutants')}
    write_json(evidence / 'tools.json', {name: dict(path=str(Path(path).resolve()), sha256=digest(Path(path)))
                                      for name, path in executables.items()})
    run([env['CARGO'], 'generate-lockfile', '--offline'], fixture, env, evidence / 'lockfile.log')
    paths.append('Cargo.lock')
    run(['git', '-c', 'init.defaultBranch=canary', 'init', '.'], fixture, env, evidence / 'git-init.log')
    run(['git', 'add', *paths], fixture, env, evidence / 'git-add.log')
    run(['git', '-c', 'user.name=Mutation Canary', '-c', 'user.email=canary@localhost', '-c', 'core.hooksPath=/dev/null',
         '-c', 'commit.gpgsign=false', 'commit', '-m', 'fixture baseline'], fixture, env, evidence / 'git-commit.log')
    tracked = {path: digest(fixture / path) for path in paths}
    fixture_digest = hashlib.sha256(json.dumps(tracked, sort_keys=True).encode()).hexdigest()
    write_json(evidence / 'fixture.json', dict(files=tracked, digest=fixture_digest))
    for path in paths:
        dest = evidence / 'fixture-source' / path
        dest.parent.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(fixture / path, dest)
    selected = {}
    baselines = []
    for case, files in [('comment', [paths[1]]), ('complete', [paths[1], paths[3]]), ('exhausted', [paths[1]])]:
        inputs = evidence / (case + '-inputs')
        inputs.mkdir()
        diff = ''
        for path in files:
            current = (fixture / path).read_text()
            old = current.replace('comment after', 'comment before') if case == 'comment' else current.replace('body after', 'body before')
            diff += ''.join(difflib.unified_diff(old.splitlines(True), current.splitlines(True),
                                               fromfile='a/' + path, tofile='b/' + path, n=0))
        (inputs / 'input.diff').write_text(diff)
        (inputs / 'files.txt').write_text('\n'.join(files) + '\n')
        run(['python3', str(SCRIPTS / 'select-mutation-packages.py'), str(inputs / 'files.txt'), str(inputs / 'packages.txt')],
            fixture, env, inputs / 'selection.log')
        packages = (inputs / 'packages.txt').read_text().splitlines()
        if packages != ([ROOT_PACKAGE] if case != 'complete' else sorted([ROOT_PACKAGE, SIBLING])):
            raise ValueError('fixture package selection mismatch')
        case_env = dict(env, NW_CARGO_TRACE=str(inputs / 'wrapper.jsonl'))
        args = ['bash', str(SCRIPTS / 'run-mutation-scope.sh'), '--packages', str(inputs / 'packages.txt'),
                '--diff', str(inputs / 'input.diff'), '--output', str(evidence / case)]
        if case != 'comment':
            if case == 'complete':
                for package in (ROOT_PACKAGE, SIBLING):
                    baselines.append(run([env['CARGO'], 'test', '--locked', '-p', package], fixture, env,
                                         evidence / (package + '-baseline.log')))
                write_json(evidence / 'baselines.json', baselines)
            command = ['cargo-mutants', 'mutants']
            for package in packages:
                command += ['-p', package]
            command += ['--in-diff', str(inputs / 'input.diff'), '--list', '--json']
            with (inputs / 'discovery.json').open('w') as stdout, (inputs / 'discovery.log').open('w') as stderr:
                subprocess.run(command, cwd=fixture, env=case_env, check=True, stdout=stdout, stderr=stderr)
            discovered = json.loads((inputs / 'discovery.json').read_text())
            intended = [m for m in discovered if m['genre'] == 'FnValue' and m['replacement'] == 'false'
                        and m['function']['function_name'] == 'ready']
            if len(intended) != len(packages) or sorted(m['package'] for m in intended) != packages:
                raise ValueError('invalid fixture: expected one bool-false mutant per package')
            selected[case] = intended
            write_json(inputs / 'selected.json', intended)
            # Rust regex accepts escapes for these punctuation characters; names
            # here come from this tracked fixture, never from dispatch inputs.
            regex = '^(?:' + '|'.join(re.escape(m['name']) for m in intended).replace('\\ ', ' ') + ')$'
            (inputs / 'filter.txt').write_text(regex)
            args += ['--filter', str(inputs / 'filter.txt'), '--expected-plan', str(inputs / 'selected.json')]
        if case == 'exhausted':
            marker = evidence / 'blocked-mutant-started'
            case_env.update(MUTATION_CANARY_HOLD='1', MUTATION_CANARY_MARKER=str(marker),
                            MUTATION_CANARY_DIGEST=fixture_digest)
            args += ['--budget', '20s', '--test-timeout', '90', '--kill-grace', '5s']
        else:
            args += ['--budget', '180s']
        trace_before = len(trace_records(inputs / 'wrapper.jsonl')) if (inputs / 'wrapper.jsonl').exists() else 0
        timing = run(args, fixture, case_env, inputs / 'driver.log')
        write_json(inputs / 'timing.json', timing)
        reporter = run(['python3', str(SCRIPTS / 'report-mutation-results.py'), '--evidence', str(evidence / case)],
                       fixture, env, inputs / 'reporter.log', check=False)
        write_json(inputs / 'reporter-status.json', reporter)
        result = json.loads((evidence / case / 'result.json').read_text())
        check_counts(case, result)
        if reporter['status'] != result['exit_status']:
            raise ValueError('reporter process exit disagrees with result')
        records = trace_records(inputs / 'wrapper.jsonl')[trace_before:]
        write_json(inputs / 'mutation-trace.json', records)
        if case == 'comment':
            if any(r['original_argv'][0] in ('test', 'build', 'check', 'rustc') for r in records):
                raise ValueError('comment-only scenario compiled or tested')
        else:
            check_identity(json.loads((evidence / case / 'mutants.out/mutants.json').read_text()), selected[case])
            if case == 'complete':
                check_trace(records, result)
        if case == 'exhausted':
            pid, marker_digest, epoch = marker.read_text().split()
            status = json.loads((evidence / case / 'status.json').read_text())
            if int(pid) <= 0 or marker_digest != fixture_digest or not status['started_epoch'] <= float(epoch) < status['started_epoch'] + 20:
                raise ValueError('negative mutant did not block before the outer deadline')
        restored = all(digest(fixture / path) == value for path, value in tracked.items())
        write_json(inputs / 'restoration.json', dict(restored=restored))
        if case != 'exhausted' and not restored:
            raise ValueError('mutation source restoration failed')
    write_json(evidence / 'selected.json', selected)
    helper_paths = sorted([p for p in SCRIPTS.glob('*mutation*.py')] +
                          [SCRIPTS / 'ci-direct-cargo.sh', SCRIPTS / 'run-mutation-scope.sh',
                           ROOT / '.github/workflows/mutation-canary.yml', ROOT / '.github/workflows/ci.yml',
                           SCRIPTS / 'verify-release-required-ci.py'])
    helpers = {str(p.relative_to(ROOT)): digest(p) for p in helper_paths}
    files = {str(p.relative_to(evidence)): digest(p) for p in evidence.rglob('*') if p.is_file()}
    write_json(evidence / 'manifest.json', dict(sha=os.environ['GITHUB_SHA'], run_id=os.environ['GITHUB_RUN_ID'],
               attempt=os.environ['GITHUB_RUN_ATTEMPT'], helpers=helpers, files=files, negative_reporter_exit=1,
               fixture_digest=fixture_digest))
    # Preserve the actual negative job failure. This is intentionally not a green orchestration catch.
    return reporter['status']


def verify(evidence, upstream):
    if upstream != 'failure':
        raise ValueError('raw canary job must actually fail, not be cancelled or skipped')
    manifest = json.loads((evidence / 'manifest.json').read_text())
    for key, env in [('sha', 'GITHUB_SHA'), ('run_id', 'GITHUB_RUN_ID'), ('attempt', 'GITHUB_RUN_ATTEMPT')]:
        if manifest[key] != os.environ[env]:
            raise ValueError('stale artifact identity')
    for name, expected in manifest['files'].items():
        path = evidence / name
        if not path.resolve().is_relative_to(evidence.resolve()) or digest(path) != expected:
            raise ValueError('artifact digest mismatch')
    for name, expected in manifest['helpers'].items():
        path = ROOT / name
        if not path.resolve().is_relative_to(ROOT) or digest(path) != expected:
            raise ValueError('helper source identity mismatch')
    results = {}
    for case in ('comment', 'complete', 'exhausted'):
        # Re-run the production classifier from the authenticated raw files.
        result = subprocess.run(['python3', str(SCRIPTS / 'report-mutation-results.py'), '--evidence', str(evidence / case)])
        results[case] = json.loads((evidence / case / 'result.json').read_text())
        check_counts(case, results[case])
        if case != 'exhausted' and json.loads((evidence / (case + '-inputs/restoration.json')).read_text()) != {'restored': True}:
            raise ValueError('source restoration was not proven')
        if result.returncode != results[case]['exit_status']:
            raise ValueError('reporter exit mismatch')
    selected = json.loads((evidence / 'selected.json').read_text())
    for case in ('complete', 'exhausted'):
        check_identity(json.loads((evidence / case / 'mutants.out/mutants.json').read_text()), selected[case])
    check_trace(json.loads((evidence / 'complete-inputs/mutation-trace.json').read_text()), results['complete'])
    if any(r['original_argv'][0] in ('test', 'build', 'check', 'rustc') for r in
           json.loads((evidence / 'comment-inputs/mutation-trace.json').read_text())):
        raise ValueError('zero case compiled or tested')
    baselines = json.loads((evidence / 'baselines.json').read_text())
    if len(baselines) != 2 or any(b['status'] != 0 for b in baselines):
        raise ValueError('missing successful fixture baselines')
    reporter = json.loads((evidence / 'exhausted-inputs/reporter-status.json').read_text())
    if reporter['status'] != 1 or manifest['negative_reporter_exit'] != 1:
        raise ValueError('missing actual negative reporter failure')
    pid, marker_digest, epoch = (evidence / 'blocked-mutant-started').read_text().split()
    status = json.loads((evidence / 'exhausted/status.json').read_text())
    if int(pid) <= 0 or marker_digest != manifest['fixture_digest'] or not status['started_epoch'] <= float(epoch) < status['started_epoch'] + 20:
        raise ValueError('negative marker was absent or too late')
    print('Three synthetic infrastructure contracts verified. Product mutation coverage remains separate.')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('mode', choices=('run', 'verify'))
    parser.add_argument('--evidence', type=Path, required=True)
    parser.add_argument('--upstream-result')
    args = parser.parse_args()
    if args.mode == 'run':
        return run_cases(args.evidence.resolve())
    verify(args.evidence.resolve(), args.upstream_result)
    return 0


if __name__ == '__main__':
    raise SystemExit(main())
