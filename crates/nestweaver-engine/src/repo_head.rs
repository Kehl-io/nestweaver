//! Resolving a repository's current HEAD, and the distance from an indexed SHA.
//!
//! nw-266: this exists because `stale-check`'s two routes — the CLI's direct
//! path and the MCP/daemon tool — hand-rolled the same computation and drifted
//! apart three times:
//!
//! * nw-163, `is_stale` — the daemon path was changed and the direct path was
//!   not, so one repo answered `true` without a daemon and `false` with one.
//! * nw-256, `staleness_commits_behind` — the MCP route became `Option<u64>`
//!   and the CLI kept `unwrap_or(0)`, reporting "STALE, 0 commits behind".
//! * nw-266, `current_head` — the MCP route asks the remote for a repo with no
//!   local working tree; the CLI hardcoded `None`, so a genuinely stale repo
//!   reported `ok` and exited 0. A CI gate built on it passed.
//!
//! Each of the first two fixes left a comment asserting the routes must not
//! drift, and the next divergence appeared underneath it. A comment cannot hold
//! two implementations together; only one implementation can. So the decision
//! that keeps diverging lives HERE, and both routes call it.

/// The HEAD sha of a local working tree, or `None` if it cannot be read.
pub fn local_head(repo_path: &str) -> Option<String> {
    let mut command = std::process::Command::new("git");
    command.args(["-C", repo_path, "rev-parse", "HEAD"]);
    let output =
        crate::git_cmd::run_git_with_timeout(command, std::time::Duration::from_secs(15)).ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// The HEAD sha of a remote, via `git ls-remote`.
///
/// Works for SSH (`git@github.com:…`) and HTTPS URLs. Stderr is suppressed so
/// SSH key errors and other diagnostics do not leak into tool responses.
pub fn remote_head(url: &str) -> Option<String> {
    // nw-276: guardrails, because nw-266 made a code path that previously
    // performed NO network call start performing one — on a command the
    // codebase itself calls a CI freshness gate.
    //
    // * `GIT_TERMINAL_PROMPT=0` and `GIT_ASKPASS`/`SSH_ASKPASS` — a private
    //   repo with no usable credentials otherwise PROMPTS. In CI there is
    //   nobody to answer.
    // * `stdin(null)` — inherited stdin is what lets a prompt block forever.
    //   Suppressing the prompt and leaving stdin attached still hangs on
    //   `ssh`'s own host-key confirmation.
    // * `--exit-code` keeps "reachable but empty" distinct from "unreachable".
    //
    // A timeout is deliberately NOT implemented with a thread+kill dance here:
    // `git` honours these env vars by failing fast, and the mechanisms above
    // remove the cases that hang. `stale-check`'s callers apply their own
    // budget.
    let output = std::process::Command::new("git")
        .args(["ls-remote", "--exit-code", url, "HEAD"])
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_ASKPASS", "")
        .env("SSH_ASKPASS", "")
        .env(
            "GIT_SSH_COMMAND",
            "ssh -oBatchMode=yes -oStrictHostKeyChecking=accept-new",
        )
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    // "<sha>\tHEAD\n"
    String::from_utf8_lossy(&output.stdout)
        .split_whitespace()
        .next()
        .map(str::to_string)
}

/// Resolve the current HEAD for an indexed repo — THE decision that diverged.
///
/// * A local working tree that no longer exists on disk is unverifiable, and
///   HEAD is unknowable. `None`.
/// * A local working tree: read HEAD from disk.
/// * No local working tree: **ask the remote**. `local_root()` is `None`
///   whenever `root_path` is empty and the url is not `file://`, which is
///   precisely the case [`remote_head`] exists to serve. Returning `None` here
///   is what made the CLI report `ok` for a repo the daemon called stale.
pub fn current_head(local_missing: bool, local_root: Option<&str>, url: &str) -> Option<String> {
    if local_missing {
        return None;
    }
    match local_root {
        Some(path) => local_head(path),
        None => remote_head(url),
    }
}

/// Count commits between two shas in a local working tree.
///
/// `None` means "could not count" — a failed `git rev-list`, an unreadable
/// repo, unparseable output. Distinguishable from a real zero, which matters
/// because the caller only asks when HEAD already differs from the indexed
/// SHA: a zero there is a contradiction, not an answer (nw-256).
pub fn commits_between(repo_path: &str, from_sha: &str, to_sha: &str) -> Option<u64> {
    let mut command = std::process::Command::new("git");
    command
        .args(["-C", repo_path, "rev-list", "--count"])
        .arg(format!("{from_sha}..{to_sha}"));
    let output =
        crate::git_cmd::run_git_with_timeout(command, std::time::Duration::from_secs(15)).ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8_lossy(&output.stdout).trim().parse().ok()
}

/// Is this a well-formed 40-hex git sha?
///
/// Shared so the two routes cannot disagree about which stored values are
/// countable.
pub fn is_full_sha(value: &str) -> bool {
    value.len() == 40 && value.chars().all(|c| c.is_ascii_hexdigit())
}

/// The commit distance `stale-check` publishes for one repo.
///
/// `current_head` is already resolved. Counting runs only when that HEAD
/// differs from a full indexed SHA inside a local working tree. `None` is a
/// failed `git rev-list` (uncountable). Every other case keeps the stored
/// counter, including a real zero when HEAD and the indexed SHA match.
pub fn commits_behind(
    current_head: Option<&str>,
    local_root: Option<&str>,
    indexed_sha: &str,
    stored: u32,
) -> Option<u64> {
    match (current_head, local_root) {
        (Some(head), Some(path)) if is_full_sha(indexed_sha) && head != indexed_sha => {
            commits_between(path, indexed_sha, head)
        }
        _ => Some(u64::from(stored)),
    }
}

/// The commit distance `list-repos` publishes for one repo.
///
/// Same decision as [`commits_behind`], using the local HEAD `stale-check`
/// reads for a working tree. A remote-only row is not probed: without a
/// working tree `rev-list` cannot run, and `stale-check` keeps the stored
/// counter there too.
pub fn listing_commits_behind(repo: &nestweaver_schema::Repo) -> Option<u64> {
    let Some(path) = repo.local_root() else {
        return Some(u64::from(repo.staleness_commits_behind));
    };
    // A missing tree is the stored leftover, same as `commits_behind`. An
    // existing tree whose HEAD cannot be read is uncountable: returning the
    // stored 0 would say "current" while `stale-check` says unknown.
    if !std::path::Path::new(path).exists() {
        return Some(u64::from(repo.staleness_commits_behind));
    }
    let head = local_head(path)?;
    commits_behind(
        Some(&head),
        Some(path),
        &repo.indexed_sha,
        repo.staleness_commits_behind,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git(dir: &std::path::Path, args: &[&str]) {
        let status = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .status()
            .expect("git must be available");
        assert!(status.success(), "git {args:?} failed in {}", dir.display());
    }

    /// A repo with a working tree: HEAD comes from disk.
    #[test]
    fn a_local_working_tree_resolves_head_from_disk() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        git(&repo, &["init"]);
        git(&repo, &["config", "user.email", "t@t.com"]);
        git(&repo, &["config", "user.name", "T"]);
        std::fs::write(repo.join("a.txt"), "hello").unwrap();
        git(&repo, &["add", "a.txt"]);
        git(&repo, &["commit", "-m", "one"]);

        let head = current_head(false, Some(repo.to_str().unwrap()), "unused://url")
            .expect("a local working tree must resolve HEAD");
        assert!(is_full_sha(&head), "expected a 40-hex sha, got {head:?}");
    }

    /// nw-266: a repo with NO local working tree must ask the remote.
    ///
    /// This is the branch the CLI route hardcoded to `None`, which made a
    /// genuinely stale repo report `ok` and exit 0 — and a CI gate built on
    /// `stale-check` pass.
    ///
    /// A local bare repository is a perfectly good git remote, so this
    /// exercises the real `ls-remote` path with no network.
    #[test]
    fn no_local_working_tree_asks_the_remote() {
        let dir = tempfile::tempdir().unwrap();
        let work = dir.path().join("work");
        let bare = dir.path().join("origin.git");
        std::fs::create_dir(&work).unwrap();
        git(&work, &["init"]);
        git(&work, &["config", "user.email", "t@t.com"]);
        git(&work, &["config", "user.name", "T"]);
        std::fs::write(work.join("a.txt"), "hello").unwrap();
        git(&work, &["add", "a.txt"]);
        git(&work, &["commit", "-m", "one"]);
        git(
            dir.path(),
            &[
                "clone",
                "--bare",
                work.to_str().unwrap(),
                bare.to_str().unwrap(),
            ],
        );

        let expected = local_head(work.to_str().unwrap()).expect("work tree HEAD");
        let resolved = current_head(false, None, bare.to_str().unwrap())
            .expect("a repo with no local root must fall back to the REMOTE, not give up");
        assert_eq!(
            resolved, expected,
            "the remote's HEAD must be reported; returning None here is what let \
             a stale repo pass a freshness gate"
        );
    }

    /// The counterweight to the two above: a working tree that is GONE is
    /// genuinely unknowable, and must not be answered by guessing at a remote.
    #[test]
    fn a_missing_working_tree_resolves_to_nothing() {
        assert_eq!(
            current_head(true, Some("/nonexistent"), "unused://url"),
            None,
            "a deleted working tree makes HEAD unknowable; `status: missing` and \
             `needs_reindex` carry that truth, not a guessed sha"
        );
    }

    #[test]
    fn an_uncountable_distance_is_none_not_zero() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        git(&repo, &["init"]);
        git(&repo, &["config", "user.email", "t@t.com"]);
        git(&repo, &["config", "user.name", "T"]);
        std::fs::write(repo.join("a.txt"), "hello").unwrap();
        git(&repo, &["add", "a.txt"]);
        git(&repo, &["commit", "-m", "one"]);
        let head = local_head(repo.to_str().unwrap()).unwrap();

        assert_eq!(
            commits_between(repo.to_str().unwrap(), &head, &head),
            Some(0),
            "a real zero must still be reported as zero"
        );
        assert_eq!(
            commits_between(repo.to_str().unwrap(), &"0".repeat(40), &head),
            None,
            "an unreachable range cannot be counted, and must not be reported as 0"
        );
    }

    /// `list-repos` used to emit the stored `staleness_commits_behind` column.
    /// That column stays 0 after HEAD moves, while `stale-check` counts
    /// `indexed..HEAD`. Both surfaces must report that live distance, and a
    /// repo whose indexed SHA is still HEAD must stay 0.
    #[test]
    fn list_repos_commits_behind_matches_stale_check_when_head_is_ahead() {
        let dir = tempfile::tempdir().unwrap();
        let repo_dir = dir.path().join("repo");
        std::fs::create_dir(&repo_dir).unwrap();
        git(&repo_dir, &["init"]);
        git(&repo_dir, &["config", "user.email", "t@t.com"]);
        git(&repo_dir, &["config", "user.name", "T"]);
        git(&repo_dir, &["commit", "--allow-empty", "-m", "indexed"]);
        let path = repo_dir.to_str().unwrap();
        let indexed = local_head(path).expect("indexed HEAD");

        for message in ["two", "three", "four", "five"] {
            git(&repo_dir, &["commit", "--allow-empty", "-m", message]);
        }
        let head = local_head(path).expect("moved HEAD");
        assert_ne!(
            indexed, head,
            "the fixture must move HEAD past the indexed SHA"
        );

        let ahead = nestweaver_schema::Repo {
            uid: "repo:ahead".to_string(),
            url: format!("file://{path}"),
            indexed_sha: indexed.clone(),
            staleness_commits_behind: 0,
            instance_id: "test".to_string(),
            name: None,
            root_path: Some(path.to_string()),
        };
        let current = nestweaver_schema::Repo {
            uid: "repo:current".to_string(),
            url: format!("file://{path}"),
            indexed_sha: head.clone(),
            staleness_commits_behind: 0,
            instance_id: "test".to_string(),
            name: None,
            root_path: Some(path.to_string()),
        };

        let stale_check_ahead = commits_behind(Some(&head), Some(path), &indexed, 0);
        let stale_check_current = commits_behind(Some(&head), Some(path), &head, 0);
        assert_eq!(stale_check_ahead, Some(4));
        assert_eq!(stale_check_current, Some(0));

        let listed = crate::repos_json_with_display_name(&[ahead, current]);
        let rows = listed.as_array().expect("list-repos rows");
        assert_eq!(
            rows[0]["staleness_commits_behind"].as_u64(),
            stale_check_ahead,
            "list-repos must publish the live distance, not the stored 0"
        );
        assert_eq!(
            rows[1]["staleness_commits_behind"].as_u64(),
            stale_check_current,
            "a repo indexed at HEAD stays 0 in both commands"
        );
        assert_eq!(
            rows[0]["staleness_commits_behind_uncountable"],
            serde_json::json!(false)
        );
    }

    /// A failed `rev-list` used to leave the stored 0 in place, so list-repos
    /// read as current while stale-check read as unknown.
    #[test]
    fn list_repos_names_an_uncountable_distance_instead_of_a_stored_zero() {
        let dir = tempfile::tempdir().unwrap();
        let repo_dir = dir.path().join("repo");
        std::fs::create_dir(&repo_dir).unwrap();
        git(&repo_dir, &["init"]);
        git(&repo_dir, &["config", "user.email", "t@t.com"]);
        git(&repo_dir, &["config", "user.name", "T"]);
        git(&repo_dir, &["commit", "--allow-empty", "-m", "indexed"]);
        let path = repo_dir.to_str().unwrap();
        let head = local_head(path).expect("HEAD");
        let repo = nestweaver_schema::Repo {
            uid: "repo:uncountable".to_string(),
            url: format!("file://{path}"),
            indexed_sha: "0".repeat(40),
            staleness_commits_behind: 0,
            instance_id: "test".to_string(),
            name: None,
            root_path: Some(path.to_string()),
        };
        assert_ne!(repo.indexed_sha, head);
        assert_eq!(
            listing_commits_behind(&repo),
            None,
            "an unreachable indexed sha cannot be counted"
        );
        let listed = crate::repos_json_with_display_name(&[repo]);
        let row = &listed.as_array().expect("rows")[0];
        assert_eq!(row["staleness_commits_behind"], 0);
        assert_eq!(
            row["staleness_commits_behind_uncountable"],
            serde_json::json!(true),
            "a stored 0 must be marked uncountable: {row}"
        );
    }
}

#[cfg(test)]
mod remote_guardrail_tests {
    use super::*;

    /// nw-276: an unreachable remote must FAIL, not hang waiting for input.
    ///
    /// nw-266 made this branch perform a network call where it previously made
    /// none, on a command the codebase calls a CI freshness gate. Without
    /// `GIT_TERMINAL_PROMPT=0`, a null stdin, and SSH batch mode, a private
    /// repo with no usable credentials prompts — and in CI nobody answers.
    ///
    /// A nonexistent local path is the cheapest unreachable remote there is:
    /// no network, no waiting, and `git` still takes the same credential and
    /// prompt paths.
    #[test]
    fn an_unreachable_remote_returns_none_promptly() {
        let started = std::time::Instant::now();
        let resolved = remote_head("/nonexistent/definitely-not-a-repo.git");
        let elapsed = started.elapsed();

        assert_eq!(
            resolved, None,
            "an unreachable remote must resolve to None, not a fabricated sha"
        );
        // Generous, because this asserts "did not hang", not a latency budget.
        // A prompt-blocked `git` never returns at all.
        assert!(
            elapsed < std::time::Duration::from_secs(30),
            "resolving an unreachable remote took {elapsed:?} — it should fail \
             fast, not block on a credential or host-key prompt"
        );
    }

    /// The counterweight: the guardrails must not have broken the working case.
    /// Without this, a `remote_head` that always returned `None` would satisfy
    /// the assertion above perfectly.
    #[test]
    fn the_guardrails_do_not_break_a_reachable_remote() {
        let dir = tempfile::tempdir().unwrap();
        let work = dir.path().join("work");
        let bare = dir.path().join("origin.git");
        std::fs::create_dir(&work).unwrap();
        let git = |d: &std::path::Path, args: &[&str]| {
            let status = std::process::Command::new("git")
                .args(args)
                .current_dir(d)
                .status()
                .expect("git must be available");
            assert!(status.success(), "git {args:?} failed");
        };
        git(&work, &["init"]);
        git(&work, &["config", "user.email", "t@t.com"]);
        git(&work, &["config", "user.name", "T"]);
        std::fs::write(work.join("a.txt"), "hello").unwrap();
        git(&work, &["add", "a.txt"]);
        git(&work, &["commit", "-m", "one"]);
        git(
            dir.path(),
            &[
                "clone",
                "--bare",
                work.to_str().unwrap(),
                bare.to_str().unwrap(),
            ],
        );

        let head = remote_head(bare.to_str().unwrap())
            .expect("a reachable remote must still resolve with the guardrails on");
        assert!(is_full_sha(&head), "expected a 40-hex sha, got {head:?}");
    }
}
