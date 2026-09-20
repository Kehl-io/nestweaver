//! Standard-artifact acceptance tests. Fixture setup itself uses the daemon.
use std::process::Command;

fn run_fixture(extra: &[&str]) {
    let result = Command::new("python3")
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/support/isolated_daemon.py"
        ))
        .arg("--binary")
        .arg(env!("CARGO_BIN_EXE_nestweaver"))
        .args(extra)
        .output()
        .expect("run daemon fixture with Python 3");
    assert!(
        result.status.success(),
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
}

#[test]
fn release_daemon_fixture_bootstrap() {
    run_fixture(&[]);
}

// Actual forbidden requests are CI-only. Pure policy coverage runs locally.
#[cfg(not(feature = "ci-direct-tests"))]
#[test]
#[ignore = "CI-only forbidden-request probe"]
fn release_standard_artifact_cannot_bypass() {
    let runner = matches!(std::env::var("GITHUB_ACTIONS").as_deref(), Ok("true"))
        && std::env::var("RUNNER_TEMP").is_ok_and(|v| !v.is_empty())
        && std::env::var("RUNNER_OS").is_ok_and(|v| !v.is_empty())
        && std::env::var("GITHUB_RUN_ID").is_ok_and(|v| !v.is_empty());
    if !runner {
        panic!("CI-only rejection probe; local coverage is no_daemon_gate_tests");
    }
    run_fixture(&["--ci-policy-test"]);
}
