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
//! ## nw-285: the harness had only one assertion FORM, and missed a whole class
//!
//! `suggested_command` returns `None` unless the message contains the literal
//! `"nestweaver "`, so every assertion here silently no-opped on a remedy that
//! is an INSTRUCTION rather than an INVOCATION. A truncated database was
//! answered with "The database may be checkpointing; please retry later." —
//! a remedy nobody ran, invisible to the harness built to catch remedies nobody
//! ran, because it named no command. `assert_no_transient_advice` is the second
//! form; the four corruption rows below are what it took to notice the first
//! form was not enough. Adding rows to a harness that cannot see the defect is
//! how a check goes vacuous.
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

/// Phrases that tell the operator the condition may clear on its own.
///
/// nw-285. The harness above extracts remedies with `suggested_command`, which
/// returns `None` unless the message contains the literal `"nestweaver "`. Every
/// assertion in this file therefore no-ops on a remedy that is an INSTRUCTION
/// rather than an INVOCATION — and "please retry later" is exactly that. A
/// truncated database was answered with `The database may be checkpointing;
/// please retry later.`, which is a remedy nobody ran and which this file, built
/// to catch remedies nobody ran, could not see. This is the second assertion
/// form the harness was missing, not a fifth row.
const TRANSIENT_ADVICE: &[&str] = &[
    "retry later",
    "please retry",
    "try again later",
    "may be checkpointing",
    "wait and retry",
];

/// Assert that `message` does not tell the operator to wait for a permanent
/// condition to clear.
///
/// WHERE ELSE DOES THIS PROPERTY NEED TO HOLD? On every diagnostic for a
/// deterministic failure, which is a set no fixture can enumerate — so the
/// mechanical half lives in `src/main.rs`'s
/// `no_permanent_diagnostic_advises_waiting_it_out`, an exhaustive match over
/// `CliDiagnostic`. This function is the end-to-end half: it proves the
/// property survives the ENGINE's own text, which the variant-level check
/// cannot see because that text is interpolated at runtime.
fn assert_no_transient_advice(message: &str, context: &str) {
    for phrase in TRANSIENT_ADVICE {
        assert!(
            !message.to_lowercase().contains(phrase),
            "{context}: the remedy says {phrase:?}, but this condition is \
             deterministic — waiting changes nothing and the operator has been \
             given something to do that cannot work:\n{message}"
        );
    }
}

/// Index a two-file repo and return `(tempdir, db_path)`.
fn indexed_fixture() -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    std::fs::write(
        repo.join("a.py"),
        "def a():\n    return b()\n\ndef b():\n    return 1\n",
    )
    .unwrap();
    let db = dir.path().join("code.lbug");
    direct()
        .args(["index", "--db"])
        .arg(&db)
        .arg("--repo")
        .arg(&repo)
        .assert()
        .success();
    (dir, db)
}

/// nw-285, mode "truncated file". The storage engine reports the truncation
/// precisely — "catalog page range starts at 3567 and spans 5 pages, outside
/// the database file with 1696 pages" — and then appends its own GUESS at a
/// cause it never tested for: "The database may be checkpointing; please retry
/// later." Nothing on that path asks whether a checkpoint is running or whether
/// a writer exists, and retrying never lengthens a truncated file.
///
/// Reproduced against a real 20 MB index before the fix; the message reached
/// the user through `nestweaver::error`, which carries no `help` at all.
#[test]
fn a_truncated_database_is_not_answered_with_retry_later() {
    let (_dir, db) = indexed_fixture();
    let len = std::fs::metadata(&db).unwrap().len();
    let file = std::fs::OpenOptions::new().write(true).open(&db).unwrap();
    file.set_len(len / 3).unwrap();
    drop(file);

    let output = direct()
        .args(["dead-code", "--db"])
        .arg(&db)
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();

    assert!(
        !output.status.success(),
        "a truncated database must not answer as if it were readable: {stderr}"
    );
    assert_no_transient_advice(&stderr, "truncated database");
    assert!(
        stderr.contains("nestweaver::db_corrupt"),
        "a truncated file is corrupt, not unavailable and not a missing WAL: {stderr}"
    );
    let remedy = suggested_command(&stderr)
        .unwrap_or_else(|| panic!("no runnable remedy offered: {stderr}"));
    assert!(
        !remedy.contains('<') || remedy.contains("<archive>"),
        "the remedy must name real values, or a placeholder the message \
         explains: {remedy}"
    );
}

/// nw-285, mode "random bytes mid-file". lbug is built from source in the cargo
/// registry and its `ASSERT` macro interpolates `__FILE__`, so a mid-file
/// corruption printed
/// `Assertion failed in file "/Users/<name>/.cargo/registry/.../column.cpp" on
/// line 289: startOffsetInSegment + length <= state.metadata.numValues` — an
/// internal invariant, a build machine's home directory, and no remedy.
///
/// Two properties, because they fail independently: nothing internal leaks, AND
/// something followable is offered.
#[test]
fn mid_file_corruption_neither_leaks_a_build_path_nor_omits_a_remedy() {
    // Corruption is not one behaviour. Measured on a real 20 MB index, the same
    // 0xFF fill produces a SIGSEGV at some offsets (caught by
    // `open_crash_guard`, which already answered well), an ordinary unwinding
    // C++ exception at others (the leak), and a clean read of garbage at
    // others still. Sweeping is what makes this row non-vacuous: a single
    // offset on a small fixture lands on the path that was ALREADY correct.
    let mut failed_at_least_once = false;
    for (offset_num, offset_den, len_den) in [(1_u64, 4_u64, 10_u64), (1, 20, 3), (3, 5, 8)] {
        let (_dir, db) = indexed_fixture();
        let len = std::fs::metadata(&db).unwrap().len();
        {
            use std::io::{Seek, SeekFrom, Write};
            let mut file = std::fs::OpenOptions::new().write(true).open(&db).unwrap();
            file.seek(SeekFrom::Start(len * offset_num / offset_den))
                .unwrap();
            file.write_all(&vec![0xFF_u8; (len / len_den) as usize])
                .unwrap();
        }

        let output = direct()
            .args(["dead-code", "--db"])
            .arg(&db)
            .output()
            .unwrap();
        let combined = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );

        // The nw-285 property that already held and must keep holding.
        #[cfg(unix)]
        {
            use std::os::unix::process::ExitStatusExt;
            assert!(
                output.status.signal().is_none(),
                "opening a corrupt database must not die on a signal: {combined}"
            );
        }
        if output.status.success() {
            continue;
        }
        failed_at_least_once = true;
        assert!(
            !combined.contains(".cargo/registry"),
            "a dependency's build path reached the user, including the username \
             in it: {combined}"
        );
        assert!(
            !combined.contains("Assertion failed in file \"/"),
            "a raw C++ assertion with an absolute path reached the user: {combined}"
        );
        assert_no_transient_advice(&combined, "mid-file corruption");
        assert!(
            combined.contains("backup restore") || combined.contains("re-index"),
            "corruption must name a recovery, not just a failure: {combined}"
        );
    }
    assert!(
        failed_at_least_once,
        "no offset produced a failure, so nothing above was asserted"
    );
}

/// nw-285, mode "zero-length file". `require_openable_db` passes a zero-byte
/// `.lbug` deliberately — it is what an interrupted create leaves and what the
/// store itself initialises — and `open_read_only` does not run `init_schema`,
/// so the first query died in the engine's binder with `Table Symbol does not
/// exist`: an internal sentence, no remedy, for the one corruption mode whose
/// remedy is both obvious and safe to prescribe.
#[test]
fn a_zero_length_database_says_what_to_run() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("empty.lbug");
    std::fs::write(&db, b"").unwrap();

    let output = direct()
        .args(["dead-code", "--db"])
        .arg(&db)
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();

    assert!(
        !stderr.contains("Binder exception"),
        "the engine's binder error is not a user-facing sentence: {stderr}"
    );
    assert!(
        stderr.contains("nestweaver::db_no_schema"),
        "a zero-length database must be named as such: {stderr}"
    );
    assert!(
        stderr.contains("0 bytes"),
        "the size is the whole diagnosis and must be stated: {stderr}"
    );
    assert_no_transient_advice(&stderr, "zero-length database");
    let remedy = suggested_command(&stderr)
        .unwrap_or_else(|| panic!("no runnable remedy offered: {stderr}"));
    assert!(
        remedy.starts_with("nestweaver index"),
        "the remedy for a schema-less database is to index one: {remedy}"
    );
    // Assertion (2) of the T3 contract: it parses.
    let mut probe: Vec<String> = remedy
        .split_whitespace()
        .skip(1)
        .map(str::to_string)
        .collect();
    probe.push("--help".to_string());
    direct().args(&probe).assert().success();
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
/// in scope. Assertion (2): it parses. Assertion (4) — "run it, then re-run the
/// original, and it succeeds" — IS now made here.
///
/// It was deliberately withheld before, because `instance merge` did not update
/// the recorded instance identity (nw-264), so the round-trip would have been
/// pinning a behaviour the product did not have; the remedy string carried a
/// matching hedge and stopped short of "run this". nw-264 closed that, so the
/// round trip is the proof — and it is the only thing that can honestly justify
/// deleting the hedge. A remedy nobody executed is the exact defect class this
/// file exists to close.
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

    // Assertion (4), nw-264: RUN the remedy, then re-run the invocation that
    // failed. The merge must both consolidate the graph AND move the recorded
    // identity, or the config-less index refuses again for the same reason and
    // the remedy is one the operator can follow to no effect.
    let merged = direct()
        .args(&argv)
        .args(["--db"])
        .arg(&db)
        .output()
        .unwrap();
    assert!(
        merged.status.success(),
        "the remedy this product printed must actually run:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&merged.stdout),
        String::from_utf8_lossy(&merged.stderr)
    );

    // `instance merge` autostarts a daemon even under `NESTWEAVER_NO_DAEMON=1`
    // (it is a write path and dials the daemon), and that daemon holds the
    // write LEASE on this temp database for its idle timeout — an hour. Leaving
    // it up would make the direct re-index below fail for a reason that has
    // nothing to do with nw-264, and would leak a daemon out of the test.
    // Stopping it is best-effort: if the merge ran directly there is nothing to
    // stop, and that is not a failure.
    let _ = direct()
        .args(["daemon", "--db"])
        .arg(&db)
        .arg("stop")
        .output();

    let after = direct()
        .args(["index", "--db"])
        .arg(&db)
        .arg("--repo")
        .arg(&repo_one)
        .output()
        .unwrap();
    let after_stderr = String::from_utf8_lossy(&after.stderr).to_string();
    assert!(
        after.status.success(),
        "after the remedy the original invocation must succeed; it still \
         refuses, so the merge left the recorded identity naming the \
         merged-away instance and the fork returns on the next index \
         (nw-264):\n{after_stderr}"
    );
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

/// The five artifacts the WAL-corruption runbook names, in the order it names
/// them.
const WAL_RUNBOOK_ARTIFACTS: &[&str] = &[
    ".wal",
    ".wal.checkpoint",
    ".shadow",
    ".checkpoint.apply.lock",
    ".checkpoint.intent.lock",
];

/// Make `db` report an unreadable write-ahead log.
///
/// nw-332 said the corrupt-WAL state could not be reproduced without lbug-001,
/// which nobody can trigger. That is true of the SIGSEGV that produced it in
/// production; it is NOT true of the state it leaves behind. Garbage in
/// `<db>.wal` with no `<db>.shadow` beside it reproduces the unreadable-log open
/// failure deterministically, in milliseconds, with no crash — the engine
/// reports `Storage exception: Checksum verification failed, the WAL file is
/// corrupted.`
///
/// This shape must NOT be confused with the orphaned-WAL one the store already
/// recovers: `quarantine_orphaned_wal` fires on `.wal` present + `.shadow`
/// absent, but only when the engine's message names the missing shadow file,
/// which it does not here.
fn corrupt_the_wal(db: &std::path::Path) {
    let wal = std::path::PathBuf::from(format!("{}.wal", db.display()));
    let shadow = std::path::PathBuf::from(format!("{}.shadow", db.display()));
    let _ = std::fs::remove_file(&shadow);
    std::fs::write(&wal, vec![0xA5u8; 4096]).unwrap();
}

/// nw-332. ONE condition, ONE remedy — and the remedy is EXECUTED here, not
/// merely printed.
///
/// This condition used to reach the operator through three code paths that gave
/// three different answers, two of which made recovery harder: "start the
/// daemon" (which against this state is the crash-restart loop), "delete this
/// database and re-index" (which discards a graph five files restore), and
/// "another process holds the write lock" (naming a process that does not
/// exist). The recovery that worked was named by none of them.
#[test]
fn a_corrupt_wal_prints_a_runbook_that_actually_recovers_the_database() {
    let (dir, db) = indexed_fixture();
    let repo = dir.path().join("repo");
    corrupt_the_wal(&db);

    let output = direct()
        .args(["search", "a", "--db"])
        .arg(&db)
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    // miette hard-wraps and indents `help` text, so a path long enough to wrap
    // is split across lines mid-token. Compare on the unwrapped form: temp
    // paths contain no whitespace, so removing all of it is lossless here and
    // asserting on the wrapped text would silently weaken every check below.
    let unwrapped: String = stderr.chars().filter(|c| !c.is_whitespace()).collect();

    assert!(
        stderr.contains("nestweaver::db_wal_corrupt"),
        "an unreadable log is neither an unreplayed one nor a damaged FILE, and \
         both of those diagnostics give it the wrong remedy:\n{stderr}"
    );
    assert!(
        !stderr.contains("delete this database"),
        "the database file is intact here — the runbook below recovers it — so \
         prescribing deletion destroys a recoverable graph:\n{stderr}"
    );
    assert!(
        !stderr.contains("daemon --db") || !stderr.contains("start"),
        "starting a writer against a log that cannot be read IS the \
         crash-restart loop:\n{stderr}"
    );
    for artifact in WAL_RUNBOOK_ARTIFACTS {
        assert!(
            unwrapped.contains(&format!("{}{artifact}", db.display())),
            "the runbook is five artifacts and it must NAME them, against the \
             right database: {artifact} missing from:\n{stderr}"
        );
    }
    assert_no_transient_advice(&stderr, "an unreadable write-ahead log");

    // NOW RUN IT. A remedy nobody executed is the exact defect class this file
    // exists to close, and last round's fix for it introduced a new instance.
    let aside = dir.path().join("aside");
    std::fs::create_dir_all(&aside).unwrap();
    let mut moved = 0;
    for artifact in WAL_RUNBOOK_ARTIFACTS {
        let from = std::path::PathBuf::from(format!("{}{artifact}", db.display()));
        if from.exists() {
            std::fs::rename(&from, aside.join(artifact.trim_start_matches('.'))).unwrap();
            moved += 1;
        }
    }
    assert!(moved > 0, "step 2 must have something to move");

    // Step 3: reopen.
    let reopened = direct()
        .args(["search", "a", "--db"])
        .arg(&db)
        .output()
        .unwrap();
    assert!(
        reopened.status.success(),
        "the runbook this product prints must actually recover the \
         database:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&reopened.stdout),
        String::from_utf8_lossy(&reopened.stderr)
    );

    // Step 4: the full re-index the runbook says is REQUIRED, not optional.
    direct()
        .args(["index", "--db"])
        .arg(&db)
        .arg("--repo")
        .arg(&repo)
        .assert()
        .success();
    let after = direct()
        .args(["search", "a", "--db"])
        .arg(&db)
        .output()
        .unwrap();
    assert!(
        String::from_utf8_lossy(&after.stdout).contains("a "),
        "the graph must be readable after the runbook: {}",
        String::from_utf8_lossy(&after.stdout)
    );
    drop(dir);
}

/// nw-333. `repair` attributed EVERY read-write open failure to lock
/// contention. This fixture holds no lock at all — the write-ahead log is
/// unreadable — so the message named a process that does not exist and
/// prescribed a daemon restart, which against a damaged database is what
/// produces the crash-restart loop. Its error was also a bare `eprintln!`, so
/// the `CliDiagnostic` inventory could not see it.
#[test]
fn repair_does_not_blame_a_lock_nobody_holds() {
    let (dir, db) = indexed_fixture();

    // A dirty publication marker with a writer that is provably gone, so
    // `repair` gets past its CLEAN early return and reaches the open.
    let marker = std::path::PathBuf::from(format!("{}.index-dirty", db.display()));
    std::fs::write(&marker, r#"{"writer_pid":999999,"reason":"index"}"#).unwrap();
    corrupt_the_wal(&db);

    let output = direct().args(["repair", "--db"]).arg(&db).output().unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();

    assert!(
        !stderr.contains("Another process holds the write lock"),
        "no process holds this database; the open failed because the LOG is \
         unreadable, and the message said so in its own parenthetical before \
         ignoring it:\n{stderr}"
    );
    assert!(
        !stderr.contains("then start"),
        "restarting the daemon against a damaged database is what produces the \
         crash-restart loop — the one remedy in this class that makes recovery \
         HARDER rather than merely wasting time:\n{stderr}"
    );
    assert!(
        stderr.contains("nestweaver::db_wal_corrupt"),
        "the open failure must reach the operator as a classified diagnostic, \
         not a bare eprintln the CliDiagnostic inventory cannot see:\n{stderr}"
    );
    assert_no_transient_advice(&stderr, "repair on a damaged database");
    drop(dir);
}

/// The other half, and what keeps the first from over-correcting: a repair
/// blocked by a REAL lock must still say so. Without this, "delete the
/// sentence" passes.
#[test]
fn repair_still_names_a_lock_that_is_genuinely_held() {
    let (dir, db) = indexed_fixture();
    let marker = std::path::PathBuf::from(format!("{}.index-dirty", db.display()));
    std::fs::write(&marker, r#"{"writer_pid":999999,"reason":"index"}"#).unwrap();

    let _holder =
        nestweaver_store::GraphStore::open(&db).expect("a healthy fixture must open read-write");

    let output = direct().args(["repair", "--db"]).arg(&db).output().unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    assert!(
        stderr.contains("write lock"),
        "a genuinely held lock must still be reported as one: {stderr}"
    );
    drop(dir);
}
