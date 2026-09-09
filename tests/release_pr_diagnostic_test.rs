//! nw-465: the `release-pr` output diagnostic must distinguish a legitimately
//! absent release PR from a lost job output.
//!
//! Both arrive at the consumer as `result=success, number=""`. The nw-426
//! diagnostic failed on that pair alone, so a successful v9.3.0 publication
//! (run 34187421838 — all four archives, all five npm packages verified) was
//! reported red. These tests pin the four cases the classifier must separate.

use std::process::{Command, Output};

fn classify(vars: &[(&str, &str)]) -> Output {
    let mut cmd = Command::new("bash");
    cmd.arg("scripts/classify-release-pr-outputs.sh")
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        // Inherit nothing that could mask a missing variable.
        .env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default());
    for (k, v) in vars {
        cmd.env(k, v);
    }
    cmd.output().expect("classifier failed to spawn")
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// Case 1 — no release PR after a release. THE nw-465 REGRESSION.
/// The producer looked, correctly found none, and exited 0. A red workflow here
/// is the bug: publication succeeded.
#[test]
fn no_release_pr_after_release_is_not_a_failure() {
    let out = classify(&[
        ("RESULT", "success"),
        ("LOCATED", "false"),
        ("NUMBER", ""),
        ("BRANCH", ""),
        ("HEAD_SHA", ""),
    ]);
    assert!(
        out.status.success(),
        "a successful publication that leaves no open release PR must not fail the \
         diagnostic; stderr: {}",
        stderr(&out)
    );
    assert!(
        stdout(&out).contains("no open release PR"),
        "the valid no-PR state must be STATED, not merely silent — otherwise it is \
         indistinguishable from a check that did not run; stdout: {}",
        stdout(&out)
    );
}

/// Case 2 — a located PR whose outputs all arrived.
#[test]
fn located_pr_with_complete_outputs_passes() {
    let out = classify(&[
        ("RESULT", "success"),
        ("LOCATED", "true"),
        ("NUMBER", "372"),
        ("BRANCH", "release-please--branches--main"),
        ("HEAD_SHA", "572fe215ca63f9b90c0b76e96bcc572fb461d2ef"),
    ]);
    assert!(
        out.status.success(),
        "a fully propagated release PR must pass; stderr: {}",
        stderr(&out)
    );
    assert!(stdout(&out).contains("372"), "stdout: {}", stdout(&out));
}

/// Case 3 — the nw-426 fault itself. The producer located a PR and the number
/// did not survive. This MUST still fail; nw-465 must not buy its fix by
/// blinding the original detector.
#[test]
fn located_pr_with_lost_number_still_fails() {
    let out = classify(&[
        ("RESULT", "success"),
        ("LOCATED", "true"),
        ("NUMBER", ""),
        ("BRANCH", "release-please--branches--main"),
        ("HEAD_SHA", "572fe215ca63f9b90c0b76e96bcc572fb461d2ef"),
    ]);
    assert!(
        !out.status.success(),
        "a located PR with a lost number is nw-426 reproducing and must fail"
    );
    assert!(
        stderr(&out).contains("nw-426"),
        "the failure must name the fault it detected; stderr: {}",
        stderr(&out)
    );
}

/// Partial identity loss is the same class: a located PR that reaches the
/// lockfile job without a head SHA cannot have its exact-head lease taken.
#[test]
fn located_pr_with_lost_head_sha_fails() {
    let out = classify(&[
        ("RESULT", "success"),
        ("LOCATED", "true"),
        ("NUMBER", "372"),
        ("BRANCH", "release-please--branches--main"),
        ("HEAD_SHA", ""),
    ]);
    assert!(
        !out.status.success(),
        "a located PR missing head_sha must fail: the exact-head lease cannot be taken"
    );
}

/// Case 4 — the producer failed. Its own job is already red and names the
/// cause; a second red job with a vaguer message adds noise, not signal.
#[test]
fn producer_failure_is_reported_by_the_producer_not_here() {
    for result in ["failure", "cancelled", "skipped"] {
        let out = classify(&[
            ("RESULT", result),
            ("LOCATED", ""),
            ("NUMBER", ""),
            ("BRANCH", ""),
            ("HEAD_SHA", ""),
        ]);
        assert!(
            out.status.success(),
            "result={result} must not raise a second failure here; stderr: {}",
            stderr(&out)
        );
    }
}

/// The producer's belief and its outputs contradicting each other is a real
/// fault in the other direction, and was undetectable before `located` existed.
#[test]
fn number_without_a_located_pr_is_a_contradiction() {
    let out = classify(&[
        ("RESULT", "success"),
        ("LOCATED", "false"),
        ("NUMBER", "372"),
        ("BRANCH", ""),
        ("HEAD_SHA", ""),
    ]);
    assert!(
        !out.status.success(),
        "reporting no PR while propagating a PR number is contradictory and must fail"
    );
}

/// `located` is itself a job output. If the mapping loses it, passing would
/// reintroduce the blind spot one level up — so an absent value is a fault, not
/// a default.
#[test]
fn missing_located_output_is_itself_treated_as_output_loss() {
    let out = classify(&[
        ("RESULT", "success"),
        ("LOCATED", ""),
        ("NUMBER", ""),
        ("BRANCH", ""),
        ("HEAD_SHA", ""),
    ]);
    assert!(
        !out.status.success(),
        "a missing `located` output must fail rather than silently pass"
    );
    assert!(
        stderr(&out).contains("located"),
        "stderr: {}",
        stderr(&out)
    );
}
