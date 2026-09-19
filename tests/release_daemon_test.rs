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
    if !["CI", "GITHUB_ACTIONS"]
        .iter()
        .any(|key| matches!(std::env::var(key).as_deref(), Ok("1" | "true")))
    {
        panic!("CI-only rejection probe; local coverage is no_daemon_gate_tests");
    }
    run_fixture(&["--ci-policy-test"]);
}
