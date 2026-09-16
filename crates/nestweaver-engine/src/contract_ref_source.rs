//! Git-ref-aware spec source loader for `contracts diff` (nw-215 d / Task
//! 4.3b, owner decision Q3: git refs on both surfaces).
//!
//! [`nestweaver_engine::contracts::diff_openapi`](crate::contracts::diff_openapi)
//! already compares two spec SOURCE STRINGS; it has no opinion on where those
//! strings came from. This module is the ONE place that turns a caller-
//! supplied ref/path into a source string, so the CLI (`contracts diff --base
//! <ref>:<path>`) and MCP (`contracts_diff(repo, path, base_ref, head_ref)`)
//! surfaces read specs identically instead of hand-rolling the same git
//! plumbing twice and drifting apart (the fate `repo_head.rs` documents for
//! `stale-check`).
//!
//! # Security model
//!
//! A ref or path reaching this module may be fully attacker/agent-controlled
//! (an MCP tool argument, or a CLI flag echoing PR review automation). Three
//! things must never happen:
//!
//! 1. **Option/argument injection into `git`.** A ref like
//!    `--upload-pack=/some/script` must never reach `git` where it could be
//!    read as a flag. [`verify_git_ref`] rejects any ref beginning with `-`
//!    outright, and separately resolves every ref through
//!    [`crate::git_diff::resolve_commit_ref`], which passes `--end-of-options`
//!    before the revision so `git rev-parse` can never reinterpret it as a
//!    flag either way. Only the verified, hex-only commit object id that
//!    comes back is ever interpolated into a later `git show` argument — the
//!    raw ref text is not.
//! 2. **Path traversal.** A path like `../../etc/passwd` or an absolute path
//!    must never resolve outside the repository. [`validate_relative_spec_path`]
//!    rejects both cases outright, for every read.
//! 3. **Symlink escape (working tree only).** A path that is textually clean
//!    but passes through a symlink committed inside the repo, pointing
//!    outside it, must not be followed. [`read_spec_in_working_tree`]
//!    canonicalizes the joined path and re-checks it against the
//!    canonicalized repo root before reading. This check is specific to the
//!    working-tree branch: [`read_spec_at_ref`] reads from a git TREE object
//!    via `git show <oid>:<path>`, which can only ever resolve to another
//!    blob inside that same repository's history — there is no host
//!    filesystem to escape to.
//! 4. **Unbounded memory from a huge blob.** `path` (and, on the ref side,
//!    `git_ref` itself) can come straight from an MCP argument, so a
//!    `ref:path` naming a huge blob ANYWHERE in the repository's history —
//!    not just the working tree — must not make the daemon buffer all of it.
//!    [`MAX_CONTRACT_SPEC_BYTES`] (10 MiB) is checked BEFORE the content read
//!    in both branches: `git cat-file -s` for a ref (so `git show` never
//!    fully reads an oversized blob), `std::fs::metadata` for the working
//!    tree.
//!
//! No caller-supplied string is ever passed to a shell — every invocation
//! goes through [`std::process::Command`] with explicit argv, via the shared
//! [`crate::git_cmd::run_git_with_timeout`] helper, so there is no shell
//! metacharacter surface at all (`;`, `$(...)`, backticks are inert byte
//! sequences in an argv slot, not commands).

use std::path::{Component, Path};
use std::process::Command;

use anyhow::{Context, Result, bail};

use crate::git_cmd::{git_net_timeout, run_git_with_timeout};

/// Hard ceiling on the size of spec content this module will read, from
/// either the working tree or a git ref. `contracts_diff`'s `path` (and, for
/// the ref side, the ref itself) can be fully caller-controlled through MCP,
/// so a `ref:path` pointing at a huge blob ANYWHERE in the repository's
/// history — not just whatever happens to be checked out — must not make the
/// daemon buffer all of it into memory. 10 MiB comfortably covers real-world
/// OpenAPI/proto/GraphQL specs with headroom; [`crate::contracts::diff_openapi`]
/// only inspects top-level shape anyway, so nothing legitimate needs more.
pub const MAX_CONTRACT_SPEC_BYTES: u64 = 10 * 1024 * 1024;

/// Reject a spec path that is absolute or contains a `..` component.
///
/// Deliberately syntactic (component-based), not a post-hoc "does the
/// resolved path still start with the root" check — the plan requires
/// rejecting `..` outright rather than only when it nets out escaping, so
/// `foo/../bar` is rejected even though it would resolve inside the root.
/// (The working-tree reader still does a *second*, resolution-based check on
/// top of this one, to also catch symlink escape — see module docs.)
fn validate_relative_spec_path(path: &str) -> Result<()> {
    let candidate = Path::new(path);
    if candidate.is_absolute() {
        bail!("spec path must be relative to the repository root, got an absolute path: {path:?}");
    }
    if candidate
        .components()
        .any(|c| matches!(c, Component::ParentDir))
    {
        bail!("spec path must not contain '..' (path traversal): {path:?}");
    }
    if path.is_empty() {
        bail!("spec path must not be empty");
    }
    Ok(())
}

/// Verify that `git_ref` resolves to a real commit inside `repo_root`, and
/// return the resolved commit object id — never the raw ref text — for use
/// by [`read_spec_at_ref`].
///
/// The real security boundary is [`crate::git_diff::resolve_commit_ref`],
/// which crosses the same
/// `git rev-parse --verify --quiet --end-of-options <ref>^{commit}` line
/// `brain_diff.since_sha` already does — `--end-of-options` means git can
/// never read `<ref>` as a flag regardless of its shape. The explicit
/// leading-`-` check below runs BEFORE that and is pure defense in depth: it
/// rejects the common injection shape (`-x`, `--upload-pack=...`) with a
/// fast, precise error before a subprocess is even spawned. It is not what
/// makes this safe, and it does not need to cover every case — e.g. a ref
/// with a leading space (`" -x"`) sails past this check untouched, but still
/// fails at `resolve_commit_ref`, because git itself does not resolve it to
/// a real commit (whitespace is not valid revision syntax). Both paths land
/// on the same `Err`.
pub fn verify_git_ref(repo_root: &Path, git_ref: &str) -> Result<String> {
    if git_ref.starts_with('-') {
        bail!("git ref must not begin with '-': {git_ref:?}");
    }
    if git_ref.is_empty() {
        bail!("git ref must not be empty");
    }
    crate::git_diff::resolve_commit_ref(repo_root, git_ref)
        .with_context(|| format!("resolve git ref {git_ref:?} in {}", repo_root.display()))
}

/// Read a spec file from the working tree, confined to `repo_root`.
///
/// `path` is validated with [`validate_relative_spec_path`], then joined onto
/// `repo_root` and canonicalized; the canonical result must still start with
/// the canonicalized `repo_root`, closing the symlink-escape gap a purely
/// syntactic check on `path` alone would miss.
///
/// **TOCTOU note:** the escape check (canonicalize + `starts_with`), the
/// subsequent size check, and the final read are three separate syscalls,
/// not one atomic operation. Between them, `path` could in principle be
/// swapped out from under this function — e.g. replaced with a symlink
/// escaping the root, or grown past the size limit — by something else
/// writing to the repository concurrently. This is accepted under this
/// tool's threat model: a single local operator/daemon process reading its
/// own working tree, not a multi-tenant sandbox racing an adversarial,
/// concurrently-mutating filesystem. Closing it properly would need an
/// `openat`-style fd-relative walk, which is more machinery than this
/// review-time diff tool warrants.
pub fn read_spec_in_working_tree(repo_root: &Path, path: &str) -> Result<String> {
    read_spec_in_working_tree_with_limit(repo_root, path, MAX_CONTRACT_SPEC_BYTES)
}

/// [`read_spec_in_working_tree`] with an explicit size ceiling — split out so
/// tests can exercise the refusal path with a tiny limit instead of writing a
/// real `MAX_CONTRACT_SPEC_BYTES` (10 MiB) fixture.
fn read_spec_in_working_tree_with_limit(
    repo_root: &Path,
    path: &str,
    limit: u64,
) -> Result<String> {
    validate_relative_spec_path(path)?;
    let joined = repo_root.join(path);
    let canonical_root = repo_root
        .canonicalize()
        .with_context(|| format!("resolve repository root {}", repo_root.display()))?;
    let canonical_target = joined
        .canonicalize()
        .with_context(|| format!("resolve spec path {}", joined.display()))?;
    if !canonical_target.starts_with(&canonical_root) {
        bail!(
            "spec path escapes the repository root: {path:?} resolves to {} which is outside {}",
            canonical_target.display(),
            canonical_root.display()
        );
    }
    let metadata =
        std::fs::metadata(&canonical_target).with_context(|| format!("stat spec file {path:?}"))?;
    let size = metadata.len();
    if size > limit {
        bail!(
            "spec file {path:?} is {size} bytes, over the {limit}-byte limit (MAX_CONTRACT_SPEC_BYTES)"
        );
    }
    std::fs::read_to_string(&canonical_target)
        .with_context(|| format!("read spec file {}", canonical_target.display()))
}

/// Read a spec file's content at a specific git revision via
/// `git show <verified_oid>:<path>`.
///
/// `verified_oid` MUST be the output of [`verify_git_ref`] — never a raw,
/// caller-supplied ref string — so this function can only ever be invoked
/// with a validated hex commit object id (enforced defensively below too).
/// `path` is validated the same way as [`read_spec_in_working_tree`] before
/// being combined into the single `<oid>:<path>` git argument. That combined
/// argument can never be read as an option: it is prefixed by a validated hex
/// object id, which cannot start with `-`.
///
/// Deliberately NOT preceded by a `--` separator: unlike a bare pathspec,
/// `git show` treats a `--` ahead of a `<tree-ish>:<path>` OBJECT spec as
/// "everything after this is a pathspec, not a revision", which silently
/// reinterprets it as `git show -- <path>` (a diff of that literal working-
/// directory path at the default revision) instead of printing the blob —
/// confirmed empirically: `git show -- HEAD~1:openapi.yaml` exits 0 with
/// EMPTY stdout, while `git show HEAD~1:openapi.yaml` prints the blob. `--`
/// is also unnecessary here for the injection concern `--` normally guards:
/// the argument is prefixed by a validated hex object id, which can never be
/// read as an option in the first place.
pub fn read_spec_at_ref(repo_root: &Path, verified_oid: &str, path: &str) -> Result<String> {
    read_spec_at_ref_with_limit(repo_root, verified_oid, path, MAX_CONTRACT_SPEC_BYTES)
}

/// [`read_spec_at_ref`] with an explicit size ceiling — split out so tests
/// can exercise the refusal path with a tiny limit instead of committing a
/// real `MAX_CONTRACT_SPEC_BYTES` (10 MiB) blob into a fixture repo.
fn read_spec_at_ref_with_limit(
    repo_root: &Path,
    verified_oid: &str,
    path: &str,
    limit: u64,
) -> Result<String> {
    validate_relative_spec_path(path)?;
    if verified_oid.is_empty() || !verified_oid.bytes().all(|b| b.is_ascii_hexdigit()) {
        bail!(
            "internal error: verified_oid must be a validated hex commit object id, got {verified_oid:?}"
        );
    }
    let spec = format!("{verified_oid}:{path}");
    let size = git_blob_size(repo_root, &spec)?;
    if size > limit {
        bail!(
            "spec blob {spec:?} is {size} bytes, over the {limit}-byte limit (MAX_CONTRACT_SPEC_BYTES)"
        );
    }
    let mut cmd = Command::new("git");
    cmd.arg("show").arg(&spec).current_dir(repo_root);
    let output = run_git_with_timeout(cmd, git_net_timeout())
        .with_context(|| format!("run git show {spec:?} in {}", repo_root.display()))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!(
            "git show {spec:?} failed in {}: {}",
            repo_root.display(),
            stderr.trim()
        );
    }
    String::from_utf8(output.stdout)
        .with_context(|| format!("git show {spec:?} returned non-UTF-8 content"))
}

/// Return the size in bytes of the blob named by `spec` (a `<oid>:<path>`
/// object spec, already built from a verified oid), via
/// `git cat-file -s <spec>`. Run BEFORE `git show` so a blob over the size
/// ceiling is never fully read into memory in the first place — `git show`
/// buffers its whole stdout, but `cat-file -s` only asks the object database
/// for the object's header.
fn git_blob_size(repo_root: &Path, spec: &str) -> Result<u64> {
    let mut cmd = Command::new("git");
    cmd.arg("cat-file")
        .arg("-s")
        .arg(spec)
        .current_dir(repo_root);
    let output = run_git_with_timeout(cmd, git_net_timeout())
        .with_context(|| format!("run git cat-file -s {spec:?} in {}", repo_root.display()))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!(
            "git cat-file -s {spec:?} failed in {}: {}",
            repo_root.display(),
            stderr.trim()
        );
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    stdout.trim().parse::<u64>().with_context(|| {
        format!("git cat-file -s {spec:?} returned a non-numeric size: {stdout:?}")
    })
}

/// Load spec content for one side of a `contracts diff` (base or head).
///
/// `ref_arg = None` reads the working tree file — the MCP `head_ref`
/// default. `ref_arg = Some(r)` verifies `r` resolves to a real commit in
/// `repo_root` (rejecting anything option-like) and reads the spec from that
/// commit's tree. Both surfaces call this ONE function so a ref/path
/// validated one way can never be read a different, less-safe way on the
/// other surface.
pub fn load_spec_source(repo_root: &Path, ref_arg: Option<&str>, path: &str) -> Result<String> {
    match ref_arg {
        Some(git_ref) => {
            let oid = verify_git_ref(repo_root, git_ref)?;
            read_spec_at_ref(repo_root, &oid, path)
        }
        None => read_spec_in_working_tree(repo_root, path),
    }
}

/// Outcome of classifying a CLI `contracts diff --base`/`--head` argument as
/// a possible `<ref>:<path>` spec (the buf/oasdiff convention).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CliSpecArg {
    /// The text before the first `:` resolved as a real git revision inside
    /// the repo. Carries the verified commit object id (never the raw ref
    /// text) and the path after the colon, ready for [`read_spec_at_ref`].
    GitRef { oid: String, path: String },
    /// No usable `<ref>:` prefix was found — the caller should treat the
    /// ENTIRE original argument as a plain filesystem path, exactly as
    /// `contracts diff` did before this feature existed.
    PlainPath,
}

/// Classify one CLI spec argument.
///
/// Splits on the FIRST `:` — a pure string operation, so this never spawns a
/// subprocess just to look at the shape of the input. If the left side is
/// non-empty and resolves as a git revision inside `repo_root` (via
/// [`verify_git_ref`]), the input is a `<ref>:<path>` spec and the resolved
/// commit id is returned. Otherwise — no colon, an empty side, or a candidate
/// that does not resolve as a revision (including a Windows drive letter like
/// `C` in `C:\spec.yaml`, or an option-like candidate such as
/// `--upload-pack=x`, which [`verify_git_ref`] rejects) — the caller should
/// read `raw` as a plain path, unchanged. This is what keeps
/// `contracts diff --base spec.yaml` (no colon at all) and a Windows-style
/// absolute path both working exactly as before.
///
/// `repo_root: None` (no discoverable git repo for the CLI's current
/// directory) always yields `PlainPath`: there is nothing to resolve a ref
/// against.
///
/// **This is deliberately ambiguous with a literal filename in one
/// direction, matching buf/oasdiff.** If a branch or tag named `main` exists,
/// `contracts diff --base main:openapi.yaml` is ALWAYS read as
/// `ref="main"`, `path="openapi.yaml"` — never as a literal file named
/// `main:openapi.yaml` on disk, even if such a file exists. A caller who
/// genuinely wants that literal filename must rename it or pass a path that
/// doesn't parse as `<existing-ref>:<something>` (e.g. `./main:openapi.yaml`
/// falls back to `PlainPath`, since `./main` does not resolve as a
/// revision). This mirrors `buf breaking --against` and `oasdiff <rev>:<path>`,
/// which make the same trade for the same reason: without it, EVERY
/// `<ref>:<path>`-shaped argument would need extra syntax to disambiguate
/// from a same-shaped filename, defeating the point of the convention.
pub fn classify_cli_spec_arg(repo_root: Option<&Path>, raw: &str) -> CliSpecArg {
    let Some(repo_root) = repo_root else {
        return CliSpecArg::PlainPath;
    };
    let Some((ref_candidate, path)) = raw.split_once(':') else {
        return CliSpecArg::PlainPath;
    };
    if ref_candidate.is_empty() || path.is_empty() {
        return CliSpecArg::PlainPath;
    }
    match verify_git_ref(repo_root, ref_candidate) {
        Ok(oid) => CliSpecArg::GitRef {
            oid,
            path: path.to_string(),
        },
        Err(_) => CliSpecArg::PlainPath,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;
    use tempfile::TempDir;

    /// Initialize a scratch git repo with two commits that change
    /// `openapi.yaml`, mirroring the CLI's planned integration test
    /// (`contracts_diff_cli_accepts_git_ref_path_specs`). Returns the temp
    /// dir (kept alive by the caller) and the repo path.
    fn scratch_repo_with_two_spec_commits() -> TempDir {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path();
        let git = |args: &[&str]| {
            let output = Command::new("git")
                .args(args)
                .current_dir(repo)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "git {args:?} failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        };
        git(&["init", "-q"]);
        git(&["config", "user.email", "test@example.com"]);
        git(&["config", "user.name", "Test"]);
        git(&["config", "commit.gpgsign", "false"]);

        std::fs::write(
            repo.join("openapi.yaml"),
            "openapi: 3.0.0\ninfo:\n  title: t\n  version: '1'\npaths:\n  /a:\n    get:\n      responses:\n        '200':\n          description: ok\n",
        )
        .unwrap();
        git(&["add", "openapi.yaml"]);
        git(&["commit", "-q", "-m", "base spec"]);

        std::fs::write(
            repo.join("openapi.yaml"),
            "openapi: 3.0.0\ninfo:\n  title: t\n  version: '1'\npaths: {}\n",
        )
        .unwrap();
        git(&["add", "openapi.yaml"]);
        git(&["commit", "-q", "-m", "remove /a"]);

        tmp
    }

    // ── load_spec_source / working tree ─────────────────────────────────

    #[test]
    fn load_spec_source_reads_working_tree_file_when_ref_is_none() {
        let tmp = scratch_repo_with_two_spec_commits();
        let content = load_spec_source(tmp.path(), None, "openapi.yaml").unwrap();
        assert!(
            content.contains("paths: {}"),
            "working tree read must return the CURRENT (second-commit) content, got: {content}"
        );
    }

    #[test]
    fn read_spec_in_working_tree_matches_the_file_on_disk() {
        let tmp = scratch_repo_with_two_spec_commits();
        let content = read_spec_in_working_tree(tmp.path(), "openapi.yaml").unwrap();
        let expected = std::fs::read_to_string(tmp.path().join("openapi.yaml")).unwrap();
        assert_eq!(content, expected);
    }

    // ── load_spec_source / git ref ──────────────────────────────────────

    #[test]
    fn load_spec_source_reads_older_commit_via_head_tilde_one() {
        let tmp = scratch_repo_with_two_spec_commits();
        let base = load_spec_source(tmp.path(), Some("HEAD~1"), "openapi.yaml").unwrap();
        let head = load_spec_source(tmp.path(), Some("HEAD"), "openapi.yaml").unwrap();
        assert!(
            base.contains("/a:"),
            "HEAD~1 must still declare the /a endpoint, got: {base}"
        );
        assert!(
            !head.contains("/a:"),
            "HEAD must have the /a endpoint removed, got: {head}"
        );
        assert_ne!(base, head, "base and head content must differ");
    }

    #[test]
    fn load_spec_source_resolves_a_branch_name_ref() {
        let tmp = scratch_repo_with_two_spec_commits();
        Command::new("git")
            .args(["branch", "release"])
            .current_dir(tmp.path())
            .output()
            .unwrap();
        let content = load_spec_source(tmp.path(), Some("release"), "openapi.yaml").unwrap();
        assert!(content.contains("paths: {}"));
    }

    // ── verify_git_ref: the escaping cases the task requires ───────────

    /// Escaping case 1: a leading-dash ref must be rejected outright.
    #[test]
    fn verify_git_ref_rejects_leading_dash_ref() {
        let tmp = scratch_repo_with_two_spec_commits();
        let err = verify_git_ref(tmp.path(), "-x").unwrap_err();
        assert!(
            err.to_string().contains("must not begin with '-'"),
            "unexpected error: {err}"
        );
    }

    /// A more realistic option-injection shape: `--upload-pack=...` naming a
    /// sentinel file it must never touch.
    #[test]
    fn verify_git_ref_rejects_option_like_ref_without_touching_the_filesystem() {
        let tmp = scratch_repo_with_two_spec_commits();
        let sentinel = tmp.path().join("sentinel.bin");
        let sentinel_contents = b"do not create or modify\n";
        // Deliberately do NOT pre-create the sentinel: an injected
        // `--upload-pack` would try to RUN a program, not write a file, but
        // asserting non-existence covers the general "nothing happened"
        // property for any option-like ref.
        let malicious = format!("--upload-pack={}", sentinel.display());
        let err = verify_git_ref(tmp.path(), &malicious).unwrap_err();
        assert!(
            err.to_string().contains("must not begin with '-'"),
            "unexpected error: {err}"
        );
        assert!(
            !sentinel.exists(),
            "an option-like ref must never touch the filesystem: {}",
            sentinel.display()
        );
        let _ = sentinel_contents; // documents intent only
    }

    /// Escaping case 2 (via `read_spec_in_working_tree`, exercised at the
    /// `verify_git_ref` layer for the ref side): a ref containing spaces or
    /// shell metacharacters must fail CLEANLY — never be interpreted by a
    /// shell (there is none: `Command` execs argv directly) and never panic.
    #[test]
    fn verify_git_ref_rejects_ref_with_spaces_and_metacharacters_cleanly() {
        let tmp = scratch_repo_with_two_spec_commits();
        let sentinel = tmp.path().join("pwned.txt");
        for bad_ref in [
            "ref with spaces",
            "$(touch pwned.txt)",
            "`touch pwned.txt`",
            "HEAD; touch pwned.txt",
            "HEAD && touch pwned.txt",
        ] {
            let result = verify_git_ref(tmp.path(), bad_ref);
            assert!(
                result.is_err(),
                "metacharacter-bearing ref {bad_ref:?} must be rejected, not silently accepted"
            );
        }
        assert!(
            !sentinel.exists(),
            "no metacharacter ref may result in shell-style command execution"
        );
    }

    /// Escaping case 3: a nonexistent ref must fail cleanly (no panic, clear
    /// error), not silently produce an empty diff.
    #[test]
    fn verify_git_ref_rejects_nonexistent_ref_cleanly() {
        let tmp = scratch_repo_with_two_spec_commits();
        let err = verify_git_ref(tmp.path(), "this-ref-does-not-exist").unwrap_err();
        assert!(
            err.to_string().to_lowercase().contains("resolve git ref"),
            "error should name what failed: {err}"
        );
    }

    #[test]
    fn load_spec_source_rejects_option_like_ref() {
        let tmp = scratch_repo_with_two_spec_commits();
        assert!(load_spec_source(tmp.path(), Some("-x"), "openapi.yaml").is_err());
    }

    // ── path validation: the other escaping cases ───────────────────────

    /// Escaping case 4a: path traversal via `..`.
    #[test]
    fn read_spec_in_working_tree_rejects_path_traversal() {
        let tmp = scratch_repo_with_two_spec_commits();
        // A real file one level above the repo root, to prove a permissive
        // implementation really would have leaked it.
        std::fs::write(tmp.path().join("../secret.txt"), "top secret").unwrap_or(());
        let err = read_spec_in_working_tree(tmp.path(), "../openapi.yaml").unwrap_err();
        assert!(err.to_string().contains(".."), "unexpected error: {err}");
    }

    #[test]
    fn read_spec_at_ref_rejects_path_traversal() {
        let tmp = scratch_repo_with_two_spec_commits();
        let oid = verify_git_ref(tmp.path(), "HEAD").unwrap();
        let err = read_spec_at_ref(tmp.path(), &oid, "../openapi.yaml").unwrap_err();
        assert!(err.to_string().contains(".."), "unexpected error: {err}");
    }

    /// Escaping case 4b: an absolute path.
    #[test]
    fn read_spec_in_working_tree_rejects_absolute_path() {
        let tmp = scratch_repo_with_two_spec_commits();
        let err = read_spec_in_working_tree(tmp.path(), "/etc/passwd").unwrap_err();
        assert!(
            err.to_string().contains("absolute"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn read_spec_at_ref_rejects_absolute_path() {
        let tmp = scratch_repo_with_two_spec_commits();
        let oid = verify_git_ref(tmp.path(), "HEAD").unwrap();
        let err = read_spec_at_ref(tmp.path(), &oid, "/etc/passwd").unwrap_err();
        assert!(
            err.to_string().contains("absolute"),
            "unexpected error: {err}"
        );
    }

    /// Escaping case 4c: a symlink committed inside the repo, pointing
    /// outside the repo root. Only the working-tree reader needs to defend
    /// against this (see module docs for why `git show` doesn't).
    #[cfg(unix)]
    #[test]
    fn read_spec_in_working_tree_rejects_symlink_escaping_repo_root() {
        use std::os::unix::fs::symlink;

        let tmp = scratch_repo_with_two_spec_commits();
        let outside = TempDir::new().unwrap();
        let secret = outside.path().join("secret-spec.yaml");
        std::fs::write(&secret, "openapi: 3.0.0\n# should never be read\n").unwrap();

        let link = tmp.path().join("escape.yaml");
        symlink(&secret, &link).unwrap();

        let err = read_spec_in_working_tree(tmp.path(), "escape.yaml").unwrap_err();
        assert!(
            err.to_string().contains("escapes the repository root"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn read_spec_in_working_tree_rejects_empty_path() {
        let tmp = scratch_repo_with_two_spec_commits();
        assert!(read_spec_in_working_tree(tmp.path(), "").is_err());
    }

    // ── size ceiling (unbounded-output fix) ─────────────────────────────
    //
    // Uses `_with_limit` directly with a tiny limit rather than writing a
    // real `MAX_CONTRACT_SPEC_BYTES` (10 MiB) fixture.

    #[test]
    fn read_spec_in_working_tree_with_limit_refuses_file_over_limit() {
        let tmp = scratch_repo_with_two_spec_commits();
        // "openapi.yaml" from the fixture is well over 10 bytes.
        let err = read_spec_in_working_tree_with_limit(tmp.path(), "openapi.yaml", 10)
            .expect_err("a file over the limit must be refused");
        let msg = err.to_string();
        assert!(msg.contains("10"), "error should name the limit: {msg}");
        assert!(
            msg.contains("bytes"),
            "error should name the observed size: {msg}"
        );
    }

    /// Counterweight: a normal spec well within the limit is still read.
    #[test]
    fn read_spec_in_working_tree_with_limit_allows_file_within_limit() {
        let tmp = scratch_repo_with_two_spec_commits();
        let content =
            read_spec_in_working_tree_with_limit(tmp.path(), "openapi.yaml", 10_000).unwrap();
        assert!(content.contains("openapi: 3.0.0"));
    }

    #[test]
    fn read_spec_at_ref_with_limit_refuses_blob_over_limit() {
        let tmp = scratch_repo_with_two_spec_commits();
        let oid = verify_git_ref(tmp.path(), "HEAD").unwrap();
        let err = read_spec_at_ref_with_limit(tmp.path(), &oid, "openapi.yaml", 10)
            .expect_err("a blob over the limit must be refused");
        let msg = err.to_string();
        assert!(msg.contains("10"), "error should name the limit: {msg}");
        assert!(
            msg.contains("bytes"),
            "error should name the observed size: {msg}"
        );
    }

    /// Counterweight: a normal spec well within the limit is still read.
    #[test]
    fn read_spec_at_ref_with_limit_allows_blob_within_limit() {
        let tmp = scratch_repo_with_two_spec_commits();
        let oid = verify_git_ref(tmp.path(), "HEAD").unwrap();
        let content =
            read_spec_at_ref_with_limit(tmp.path(), &oid, "openapi.yaml", 10_000).unwrap();
        assert!(content.contains("openapi: 3.0.0"));
    }

    /// The public, default-limit entry points must still work end to end for
    /// an ordinary spec (the size check must not break the common case).
    #[test]
    fn public_readers_use_the_default_limit_and_still_read_a_normal_spec() {
        let tmp = scratch_repo_with_two_spec_commits();
        assert!(read_spec_in_working_tree(tmp.path(), "openapi.yaml").is_ok());
        let oid = verify_git_ref(tmp.path(), "HEAD").unwrap();
        assert!(read_spec_at_ref(tmp.path(), &oid, "openapi.yaml").is_ok());
    }

    // ── classify_cli_spec_arg ────────────────────────────────────────────

    #[test]
    fn classify_cli_spec_arg_splits_ref_and_path() {
        let tmp = scratch_repo_with_two_spec_commits();
        match classify_cli_spec_arg(Some(tmp.path()), "HEAD~1:openapi.yaml") {
            CliSpecArg::GitRef { oid, path } => {
                assert_eq!(path, "openapi.yaml");
                assert!(!oid.is_empty());
                assert!(oid.bytes().all(|b| b.is_ascii_hexdigit()));
            }
            other => panic!("expected GitRef, got {other:?}"),
        }
    }

    /// Counterweight: `contracts diff` must keep accepting plain file paths
    /// with no colon at all (the pre-existing behavior).
    #[test]
    fn classify_cli_spec_arg_falls_back_to_plain_path_when_no_colon() {
        let tmp = scratch_repo_with_two_spec_commits();
        assert_eq!(
            classify_cli_spec_arg(Some(tmp.path()), "openapi.yaml"),
            CliSpecArg::PlainPath
        );
    }

    /// A Windows-style absolute path (`C:\spec.yaml`) has a colon, but `C`
    /// does not resolve as a revision — must fall back to a plain path, not
    /// be misread as `ref="C"`.
    #[test]
    fn classify_cli_spec_arg_falls_back_to_plain_path_for_windows_style_paths() {
        let tmp = scratch_repo_with_two_spec_commits();
        assert_eq!(
            classify_cli_spec_arg(Some(tmp.path()), r"C:\Users\x\openapi.yaml"),
            CliSpecArg::PlainPath
        );
    }

    #[test]
    fn classify_cli_spec_arg_falls_back_to_plain_path_when_no_repo_root() {
        assert_eq!(
            classify_cli_spec_arg(None, "HEAD~1:openapi.yaml"),
            CliSpecArg::PlainPath
        );
    }

    /// Escaping case at the CLI classification layer: an option-like ref
    /// candidate must not be resolved as a ref — it falls back to treating
    /// the WHOLE string as an (almost certainly nonexistent, harmlessly
    /// erroring) plain path, never reaching git as a flag.
    #[test]
    fn classify_cli_spec_arg_rejects_option_like_ref_by_falling_back_to_plain_path() {
        let tmp = scratch_repo_with_two_spec_commits();
        assert_eq!(
            classify_cli_spec_arg(Some(tmp.path()), "--upload-pack=x:evil.yaml"),
            CliSpecArg::PlainPath
        );
    }

    #[test]
    fn classify_cli_spec_arg_falls_back_to_plain_path_for_empty_left_or_right_side() {
        let tmp = scratch_repo_with_two_spec_commits();
        assert_eq!(
            classify_cli_spec_arg(Some(tmp.path()), ":openapi.yaml"),
            CliSpecArg::PlainPath
        );
        assert_eq!(
            classify_cli_spec_arg(Some(tmp.path()), "HEAD:"),
            CliSpecArg::PlainPath
        );
    }
}
