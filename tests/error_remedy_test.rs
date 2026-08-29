//! S1/T3 — the remedy round-trip harness.
//!
//! > "Treat any string in an error that a user could paste into a terminal as
//! > an assertion, and test it like one."
//!
//! Phase 1 proved that the obvious form of this check — parse every command
//! string against the clap tree — catches ZERO of the four findings it was
//! commissioned for. `nestweaver index --repo .` parses; that is precisely the
//! problem. `nestweaver instance merge --from <one> --to <keep>` parses too;
//! `<one>` is a legal value. A parse check answers "does this flag exist?".
//! Every defect in this class answers a different question: **does running
//! this actually resolve the error?**
//!
//! So this file is a HARNESS PLUS A TABLE, not a sweep, and that narrowing is
//! stated rather than hidden. There is no mechanical way to enumerate every
//! refusal a binary can emit. Each row is hand-written, seeded with the known
//! cases, and grown one row per future bug — three lines each. The static
//! floors (T1 parse sweep, T2 env-var roles, T3b write-remedy denylist) live
//! in `src/main.rs`'s `cli_help_contract_tests`, where they need no fixture.
//!
//! Every invocation pins its daemon routing explicitly, per
//! `every_cli_invocation_pins_its_daemon_routing` in `tests/cli_test.rs`.
//! Every fixture is a temp directory with a temp database; nothing here reads
//! an inherited `NESTWEAVER_DB`.

use assert_cmd::Command;

/// Direct route. `NESTWEAVER_DB` is cleared, not inherited: a remedy test that
/// resolved its database from the developer's shell profile would be testing
/// the wrong graph — which is nw-284, the finding next door.
fn direct() -> Command {
    let mut cmd = Command::cargo_bin("nestweaver").unwrap();
    cmd.env("NESTWEAVER_NO_DAEMON", "1")
        .env("NESTWEAVER_ALLOW_NO_DAEMON", "1")
        .env_remove("NESTWEAVER_DB")
        .env_remove("NESTWEAVER_CONFIG");
    cmd
}

/// Pull the first `nestweaver ...` command out of a message, as a user would.
fn suggested_command(message: &str) -> Option<String> {
    let at = message.find("nestweaver ")?;
    let rest = &message[at..];
    let end = rest.find(['\n', '`', '"', '\'']).unwrap_or(rest.len());
    Some(
        rest[..end]
            .trim()
            .trim_end_matches(['.', ',', ';'])
            .to_string(),
    )
}

/// nw-310. A two-instance database refuses a config-less `index`, and the
/// consolidation command it prints must name REAL instances — both values are
/// in scope at the bail site.
///
/// Assertion (1) of the T3 contract: no `<placeholder>` when the values were
/// in scope. Assertion (2): it parses. Assertion (4) — "run it, then re-run
/// the original, and it succeeds" — is deliberately NOT made here: `instance
/// merge` does not yet update the recorded instance identity, so a test that
/// asserted the round-trip would be pinning a behaviour the product does not
/// have. The message says so too, which is why it stops short of "run this".
#[test]
fn multi_instance_refusal_emits_a_runnable_consolidation_command() {
    let dir = tempfile::tempdir().unwrap();
    let repo_one = dir.path().join("one");
    let repo_two = dir.path().join("two");
    for (repo, body) in [
        (&repo_one, "def a():\n    return 1\n"),
        (&repo_two, "def b():\n    return 2\n"),
    ] {
        std::fs::create_dir_all(repo).unwrap();
        std::fs::write(repo.join("m.py"), body).unwrap();
    }
    let db = dir.path().join("s3.lbug");

    for (repo, instance) in [(&repo_one, "alpha"), (&repo_two, "beta")] {
        direct()
            .args(["index", "--db"])
            .arg(&db)
            .arg("--repo")
            .arg(repo)
            .args(["--instance", instance])
            .assert()
            .success();
    }

    // Nothing stated, several instances present -> refuse.
    let output = direct()
        .args(["index", "--db"])
        .arg(&db)
        .arg("--repo")
        .arg(&repo_one)
        .output()
        .unwrap();
    assert_ne!(
        output.status.code(),
        Some(0),
        "a forked database must refuse"
    );
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();

    // The claim the message already kept.
    assert!(
        stderr.contains("alpha") && stderr.contains("beta"),
        "the refusal must name every instance: {stderr}"
    );

    // The claim it did not: a command the reader can actually run.
    let at = stderr
        .find("nestweaver instance merge")
        .unwrap_or_else(|| panic!("a consolidation command must be present: {stderr}"));
    let command = suggested_command(&stderr[at..]).expect("a consolidation command");
    assert!(
        !command.contains('<') && !command.contains('>'),
        "the consolidation command still carries unsubstituted placeholders, \
         though both names are in scope at the bail site: {command}"
    );
    assert!(
        command.contains("--from beta --to alpha"),
        "the command must name real instances: {command}"
    );

    // …and it must be a real invocation of this binary, not prose that looks
    // like one. `--help` parses the whole path without performing the merge.
    let mut argv: Vec<String> = command.split_whitespace().map(str::to_string).collect();
    argv.remove(0); // the binary name
    direct().args(&argv).arg("--help").assert().success();
}

/// nw-328. Two symbols with the same name inside ONE repo: `--repo` is already
/// set and by construction cannot separate them, so the remedy must not be
/// `--repo`.
///
/// Assertion (3) of the T3 contract: the remedy is not a flag already present
/// in the failing invocation.
#[test]
fn intra_repo_ambiguity_does_not_advise_the_flag_already_passed() {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("coyote-server");
    std::fs::create_dir_all(repo.join("src")).unwrap();
    std::fs::write(
        repo.join("src").join("number_format.py"),
        "def format_number(x):\n    return x\n",
    )
    .unwrap();
    std::fs::write(
        repo.join("src").join("base_report.py"),
        "class Base:\n    def format_number(self, x):\n        return x\n",
    )
    .unwrap();
    let db = dir.path().join("code.lbug");

    direct()
        .args(["index", "--db"])
        .arg(&db)
        .arg("--repo")
        .arg(&repo)
        .args(["--name", "coyote-server"])
        .assert()
        .success();

    let output = direct()
        .args(["impact", "--db"])
        .arg(&db)
        .args([
            "--repo",
            "coyote-server",
            "--depth",
            "1",
            "--json",
            "format_number",
        ])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&stdout) else {
        // The fixture did not produce a collision on this parser; say so
        // rather than passing vacuously.
        panic!("expected JSON from `impact --json`, got: {stdout}");
    };
    if value["status"] != "ambiguous" {
        panic!(
            "the fixture must produce an intra-repo collision for this test to \
             mean anything; got status {:?}",
            value["status"]
        );
    }

    let note = value["note"].as_str().unwrap_or_default();
    assert!(
        !note.contains("--repo <name>"),
        "`--repo coyote-server` was already passed and every match is inside \
         it, so re-advising --repo is a closed loop: {note}"
    );
    assert!(
        note.to_lowercase().contains("uid"),
        "the remedy must offer something that can discriminate: {note}"
    );
    // The remedy names UIDs, so the candidates must carry them.
    assert!(
        value["candidates"][0]["uid"]
            .as_str()
            .unwrap_or_default()
            .starts_with("sym:"),
        "the candidate list must carry the UID the remedy tells the user to \
         pass: {stdout}"
    );
}

/// nw-329. A lookup that found nothing IN ONE FILE must say so, and must not
/// be rewritten into a false claim about the whole database carrying
/// `nestweaver index --repo .` as its remedy — one keystroke from the
/// invocation that wrote to the production graph during the hunt.
#[test]
fn a_file_with_no_symbols_reports_the_file_not_the_database() {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    std::fs::write(repo.join("real.py"), "def real():\n    return 1\n").unwrap();
    // Parsed, indexed, and yields no symbols of its own.
    std::fs::write(repo.join("blank.py"), "# only a comment\n").unwrap();
    let db = dir.path().join("code.lbug");

    direct()
        .args(["index", "--db"])
        .arg(&db)
        .arg("--repo")
        .arg(&repo)
        .assert()
        .success();

    let output = direct()
        .args(["context", "--db"])
        .arg(&db)
        .arg(repo.join("blank.py"))
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();

    assert!(
        !stderr.contains("index --repo ."),
        "a per-file lookup miss must never prescribe an ambient index: {stderr}"
    );
    assert!(
        !stderr.contains("No symbols found in the database"),
        "the database holds symbols; a statement about one file was generalised \
         into a false statement about all of them: {stderr}"
    );
    assert!(
        stderr.contains("blank.py") || stderr.contains("No matching symbols"),
        "the error must name the scope it actually searched: {stderr}"
    );
}

/// nw-310, structurally. The four sites that render this sentence must all go
/// through one renderer; a fifth hand-written copy is how the two most recent
/// ones drifted into placeholders in the first place.
#[test]
fn no_source_file_hand_writes_a_placeholder_consolidation_command() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut offenders = Vec::new();
    let mut scanned = 0usize;
    let mut stack = vec![root.join("src"), root.join("crates")];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .to_string();
            if path.is_dir() {
                if name != "target" && name != "node_modules" {
                    stack.push(path);
                }
                continue;
            }
            if path.extension().and_then(|e| e.to_str()) != Some("rs") {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            // The WHOLE file. Cutting at the first `#[cfg(test)]` looked
            // reasonable and was not: in `src/main.rs` the first one is at
            // line 548 of 29,000, so the scan covered 2% of the file and
            // passed while the offending sites sat untouched. Measured, not
            // assumed — the truncating version passed against a tree with the
            // placeholder restored.
            scanned += 1;
            for (index, line) in text.lines().enumerate() {
                if line.trim_start().starts_with("//") {
                    continue;
                }
                if line.contains("instance merge --from <") {
                    offenders.push(format!("{}:{}", path.display(), index + 1));
                }
            }
        }
    }
    offenders.sort();
    assert!(
        scanned > 100,
        "the scan found only {scanned} source files; it is not walking the \
         workspace and would pass vacuously"
    );
    assert!(
        offenders.is_empty(),
        "these sites emit a consolidation command with unsubstituted \
         placeholders. Call \
         `nestweaver_engine::instance_remedy::instance_consolidation_remedy` \
         instead — it is the one renderer, and the instance names are in scope \
         at every one of these sites: {offenders:?}"
    );
}
