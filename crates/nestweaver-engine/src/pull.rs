use std::path::{Path, PathBuf};
use std::process::Command;

pub enum PullMode {
    Sparse { files: Vec<String> },
    Full,
}

pub enum ShaPolicy {
    Head,
    Pinned(String),
}

pub struct PullOptions {
    pub mode: PullMode,
    pub sha_policy: ShaPolicy,
    pub ephemeral: bool,
}

pub struct PullResult {
    pub path: PathBuf,
    pub head_sha: String,
    pub drift_commits: Option<u32>,
}

pub fn resolve_path(workspace_root: &Path, repo_name: &str, file_path: &str) -> PathBuf {
    workspace_root.join(repo_name).join(file_path)
}

/// Derive a repo's display/identity basename from its URL: the trailing path
/// segment with any `.git` suffix stripped and path separators sanitized.
///
/// This is the user-facing basename used for display, name matching, and
/// scheduler ids when a repo has no explicit `name` override (see
/// [`crate::repo_display_name`]). It is intentionally NOT unique across hosts
/// or orgs that share a basename — for naming an on-disk clone directory use
/// [`clone_dir_name_from_url`] instead.
pub fn repo_name_from_url(url: &str) -> String {
    let raw = url
        .rsplit('/')
        .next()
        .unwrap_or(url)
        .trim_end_matches(".git");
    raw.replace("..", "")
        .chars()
        .map(|c| if c == '/' || c == '\\' { '_' } else { c })
        .collect::<String>()
}

/// Derive the on-disk clone-directory name for a repo URL: the sanitized
/// basename plus a short hash of the *full* URL so distinct URLs that share a
/// basename (e.g. github.com/acme/api vs gitlab.com/vendor/api) map to distinct
/// clone directories instead of silently colliding. This value names the
/// on-disk clone dir only; it is not a persisted identity key (that role
/// belongs to `canonical_repo_id`).
pub fn clone_dir_name_from_url(url: &str) -> String {
    let sanitized = repo_name_from_url(url);
    let hash = crate::hash::blake3_hex(url);
    format!("{sanitized}_{}", &hash[..8])
}

fn validate_repo_dest(workspace_root: &Path, dest: &Path) -> Result<(), PullError> {
    std::fs::create_dir_all(dest).map_err(|e| PullError::Other(e.into()))?;
    let canonical_root = workspace_root
        .canonicalize()
        .unwrap_or_else(|_| workspace_root.to_path_buf());
    let canonical_dest = dest.canonicalize().unwrap_or_else(|_| dest.to_path_buf());
    if !canonical_dest.starts_with(&canonical_root) {
        std::fs::remove_dir_all(dest).ok();
        return Err(PullError::Other(anyhow::anyhow!(
            "repo destination escapes workspace root"
        )));
    }
    Ok(())
}

fn is_hex(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| c.is_ascii_hexdigit())
}

pub fn ensure_workspace_hygiene(workspace_root: &Path) -> Result<(), anyhow::Error> {
    std::fs::create_dir_all(workspace_root)?;
    let gitignore = workspace_root.join(".gitignore");
    if !gitignore.exists() {
        std::fs::write(gitignore, "*\n")?;
    }
    Ok(())
}

pub fn pull_repo(
    workspace_root: &Path,
    repo_url: &str,
    indexed_sha: &str,
    options: &PullOptions,
) -> Result<PullResult, PullError> {
    pull_repo_with_credentials(workspace_root, repo_url, indexed_sha, options, None)
}

/// Validate the selected instance's authentication strategy before filesystem
/// or network work. Credentials are supplied to Git's protocol, never its URL.
pub fn validate_credential_method(method: &str) -> Result<(), anyhow::Error> {
    if matches!(method, "gh" | "ssh" | "credential-helper" | "env") {
        Ok(())
    } else {
        anyhow::bail!("unsupported git credential_method '{method}'")
    }
}

fn pull_git_command(credential_method: Option<&str>) -> Command {
    let mut command = Command::new("git");
    command.env("GIT_TERMINAL_PROMPT", "0");
    match credential_method {
        Some("gh") => {
            command.args([
                "-c",
                "credential.helper=",
                "-c",
                "credential.helper=!gh auth git-credential",
            ]);
        }
        Some("ssh") => {
            command.args(["-c", "credential.helper="]);
        }
        Some("env") => {
            // An explicitly supplied askpass program is Git's native env
            // protocol. Otherwise expose GH_TOKEN only through helper stdin/
            // stdout; the token never appears in argv or the remote URL.
            command.args(["-c", "credential.helper="]);
            if std::env::var_os("GIT_ASKPASS").is_none() {
                command.args(["-c", r#"credential.helper=!f() { test "$1" = get && test -n "$GH_TOKEN" && printf '%s\n' 'username=x-access-token' "password=$GH_TOKEN"; }; f"#]);
            }
        }
        _ => {}
    }
    command
}

/// Instance-aware variant; the old public entry point retains ambient Git
/// authentication when no instance was selected.
pub fn pull_repo_with_credentials(
    workspace_root: &Path,
    repo_url: &str,
    indexed_sha: &str,
    options: &PullOptions,
    credential_method: Option<&str>,
) -> Result<PullResult, PullError> {
    if let Some(method) = credential_method {
        validate_credential_method(method).map_err(PullError::Other)?;
    }
    ensure_workspace_hygiene(workspace_root)?;
    let repo_name = clone_dir_name_from_url(repo_url);
    if repo_name.is_empty() || repo_name == "." || repo_name.contains("..") {
        return Err(PullError::Other(anyhow::anyhow!(
            "invalid repo name derived from URL: '{}'",
            repo_name
        )));
    }
    let dest = workspace_root.join(&repo_name);
    // The cleanup below may only ever wipe a dest this pull attempt created
    // (or an empty one) — never a pre-existing directory with contents.
    let dest_had_contents = dest
        .read_dir()
        .map(|mut entries| entries.next().is_some())
        .unwrap_or(false);
    validate_repo_dest(workspace_root, &dest)?;

    if dest.exists() && dest.join(".git").exists() {
        // Fetch
        let output = pull_git_command(credential_method)
            .args(["fetch", "origin"])
            .current_dir(&dest)
            .output()?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(classify_git_error(&stderr));
        }
    } else {
        // Clone
        let mut args = vec!["clone".to_string()];
        if matches!(options.mode, PullMode::Sparse { .. }) {
            args.push("--filter=blob:none".into());
            args.push("--sparse".into());
        }
        args.push("--".into());
        args.push(repo_url.to_string());
        args.push(dest.display().to_string());

        let output = pull_git_command(credential_method).args(&args).output()?;
        if !output.status.success() {
            // A failed pull must clean up the workspace dir it created —
            // validate_repo_dest pre-creates `dest`, and a partial clone must
            // not be mistaken for a real checkout on the next run. Never wipe
            // a pre-existing directory that had contents: the failed clone
            // did not create those files.
            if !dest_had_contents {
                let _ = std::fs::remove_dir_all(&dest);
            }
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(classify_git_error(&stderr));
        }
    }

    // SHA policy
    match &options.sha_policy {
        ShaPolicy::Head => {
            let output = Command::new("git")
                .args(["checkout", "HEAD"])
                .current_dir(&dest)
                .output()?;
            if !output.status.success() {
                return Err(PullError::Other(anyhow::anyhow!(
                    "git checkout HEAD failed"
                )));
            }
        }
        ShaPolicy::Pinned(sha) => {
            if !sha.chars().all(|c| c.is_ascii_hexdigit()) {
                return Err(PullError::Other(anyhow::anyhow!("invalid SHA: {}", sha)));
            }
            let output = Command::new("git")
                .args(["checkout", sha])
                .current_dir(&dest)
                .output()?;
            if !output.status.success() {
                return Err(PullError::Unavailable(format!("SHA {} not found", sha)));
            }
        }
    }

    // Sparse checkout specific files
    if let PullMode::Sparse { ref files } = options.mode
        && !files.is_empty()
    {
        let mut cmd = Command::new("git");
        cmd.args(["sparse-checkout", "set"]).current_dir(&dest);
        for f in files {
            cmd.arg(f);
        }
        let sc_output = cmd.output()?;
        if !sc_output.status.success() {
            return Err(PullError::Other(anyhow::anyhow!(
                "git sparse-checkout set failed"
            )));
        }
    }

    // Get HEAD SHA and compute drift
    let head_sha = get_head_sha(&dest)?;
    let drift = if !indexed_sha.is_empty() && head_sha != indexed_sha {
        Some(count_commits_between(&dest, indexed_sha, &head_sha).unwrap_or(0))
    } else {
        None
    };

    Ok(PullResult {
        path: dest,
        head_sha,
        drift_commits: drift,
    })
}

#[derive(Debug, thiserror::Error)]
pub enum PullError {
    #[error("unauthorized: {0}")]
    Unauthorized(String),
    #[error("unavailable: {0}")]
    Unavailable(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("{0}")]
    Other(#[from] anyhow::Error),
}

impl PullError {
    pub fn exit_code(&self) -> i32 {
        match self {
            PullError::Unauthorized(_) => 4,
            PullError::Unavailable(_) => 5,
            _ => 1,
        }
    }
}

fn classify_git_error(stderr: &str) -> PullError {
    let lower = stderr.to_lowercase();
    if lower.contains("authentication")
        || lower.contains("403")
        || lower.contains("permission denied")
    {
        PullError::Unauthorized(stderr.trim().to_string())
    } else {
        PullError::Unavailable(stderr.trim().to_string())
    }
}

fn get_head_sha(repo_dir: &Path) -> Result<String, anyhow::Error> {
    let output = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(repo_dir)
        .output()?;
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn count_commits_between(repo_dir: &Path, old: &str, new: &str) -> Result<u32, anyhow::Error> {
    if !is_hex(old) || !is_hex(new) {
        return Ok(0);
    }
    let output = Command::new("git")
        .args(["rev-list", "--count", &format!("{}..{}", old, new)])
        .current_dir(repo_dir)
        .output()?;
    Ok(String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse()
        .unwrap_or(0))
}

// Cleanup for ephemeral pulls
pub fn cleanup_repo(workspace_root: &Path, repo_url: &str) -> Result<(), anyhow::Error> {
    let name = clone_dir_name_from_url(repo_url);
    let path = workspace_root.join(&name);
    if path.exists() {
        std::fs::remove_dir_all(&path)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selected_credentials_are_command_local_and_never_embed_a_token() {
        for method in ["gh", "ssh", "credential-helper", "env"] {
            validate_credential_method(method).unwrap();
            let command = pull_git_command(Some(method));
            let args: Vec<_> = command
                .get_args()
                .map(|arg| arg.to_string_lossy().to_string())
                .collect();
            assert!(
                !args
                    .iter()
                    .any(|arg| arg.contains("https://") || arg.contains("Authorization:"))
            );
            if method == "gh" {
                assert!(
                    args.iter()
                        .any(|arg| arg == "credential.helper=!gh auth git-credential")
                );
                assert!(args.iter().any(|arg| arg == "credential.helper="));
            }
        }
        assert!(validate_credential_method("typo").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn selected_gh_helper_answers_git_credential_protocol_without_persisting_it() {
        use std::io::Write;
        use std::os::unix::fs::PermissionsExt;
        use std::process::Stdio;
        let dir = tempfile::tempdir().unwrap();
        let gh = dir.path().join("gh");
        std::fs::write(&gh, "#!/bin/sh\n[ \"$1 $2 $3\" = 'auth git-credential get' ] || exit 1\nprintf '%s\\n' username=fixture password=synthetic-test-token\n").unwrap();
        std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o700)).unwrap();
        let mut paths = vec![dir.path().to_path_buf()];
        paths.extend(std::env::split_paths(
            &std::env::var_os("PATH").unwrap_or_default(),
        ));
        let mut child = pull_git_command(Some("gh"))
            .args(["credential", "fill"])
            .current_dir(dir.path())
            .env("PATH", std::env::join_paths(paths).unwrap())
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(b"protocol=https\nhost=example.invalid\n\n")
            .unwrap();
        let output = child.wait_with_output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(String::from_utf8_lossy(&output.stdout).contains("password=synthetic-test-token"));
        assert!(!dir.path().join(".git/config").exists());
    }

    #[test]
    fn repo_name_from_https_url() {
        // The display/identity basename is the bare trailing segment, with no
        // hash suffix (that belongs to `clone_dir_name_from_url`).
        let name = repo_name_from_url("https://github.com/user/my-repo");
        assert_eq!(name, "my-repo");
    }

    #[test]
    fn repo_name_from_url_with_git_suffix() {
        let name = repo_name_from_url("https://github.com/user/my-repo.git");
        assert_eq!(name, "my-repo");
    }

    #[test]
    fn repo_name_from_ssh_url() {
        // SSH URLs like git@github.com:user/my-repo.git do have a '/' between user and repo,
        // so rsplit('/').next() correctly yields "my-repo.git", then ".git" is stripped.
        // Full SCP-style SSH URL parsing (handling the colon) can be improved later.
        let name = repo_name_from_url("git@github.com:user/my-repo.git");
        assert_eq!(name, "my-repo");
    }

    #[test]
    fn clone_dir_name_is_deterministic_for_same_url() {
        let url = "https://github.com/acme/api.git";
        assert_eq!(clone_dir_name_from_url(url), clone_dir_name_from_url(url));
    }

    #[test]
    fn clone_dir_name_disambiguates_same_basename() {
        // The display basename collides for same-basename repos from different
        // hosts/orgs, but the clone-dir name must not.
        let a_url = "https://github.com/acme/api.git";
        let b_url = "https://gitlab.com/vendor/api.git";
        assert_eq!(repo_name_from_url(a_url), repo_name_from_url(b_url));

        let a = clone_dir_name_from_url(a_url);
        let b = clone_dir_name_from_url(b_url);
        assert!(a.starts_with("api_"), "got {a}");
        assert!(b.starts_with("api_"), "got {b}");
        assert_eq!(a.len(), "api_".len() + 8);
        assert_ne!(a, b, "same-basename URLs must not share a clone dir");
    }

    #[test]
    fn resolve_path_format() {
        let p = resolve_path(Path::new("/workspace"), "my-repo", "src/main.ts");
        assert_eq!(p, PathBuf::from("/workspace/my-repo/src/main.ts"));
    }

    #[test]
    fn ensure_workspace_creates_gitignore() {
        let dir = tempfile::tempdir().unwrap();
        let ws = dir.path().join("workspace");
        ensure_workspace_hygiene(&ws).unwrap();
        assert!(ws.join(".gitignore").exists());
        let content = std::fs::read_to_string(ws.join(".gitignore")).unwrap();
        assert_eq!(content, "*\n");
    }

    #[test]
    fn classify_auth_error() {
        let err = classify_git_error("fatal: Authentication failed");
        assert!(matches!(err, PullError::Unauthorized(_)));
    }

    #[test]
    fn classify_other_error() {
        let err = classify_git_error("fatal: repository not found");
        assert!(matches!(err, PullError::Unavailable(_)));
    }

    #[test]
    fn pull_error_exit_codes() {
        assert_eq!(PullError::Unauthorized("".into()).exit_code(), 4);
        assert_eq!(PullError::Unavailable("".into()).exit_code(), 5);
    }

    #[test]
    fn cleanup_nonexistent_is_ok() {
        let dir = tempfile::tempdir().unwrap();
        assert!(cleanup_repo(dir.path(), "https://github.com/user/nonexistent").is_ok());
    }

    #[test]
    fn repo_name_strips_dotdot() {
        assert!(!repo_name_from_url("https://evil.com/..").contains(".."));
        assert!(!repo_name_from_url("https://evil.com/../../etc").contains(".."));
    }

    #[test]
    fn repo_name_sanitizes_slashes() {
        let name = repo_name_from_url("https://evil.com/foo/bar");
        assert!(!name.contains('/'));
    }

    #[test]
    fn is_hex_validates() {
        assert!(is_hex("abc123"));
        assert!(is_hex("ABC123"));
        assert!(!is_hex(""));
        assert!(!is_hex("--flag"));
        assert!(!is_hex("abc 123"));
        assert!(!is_hex("abc;rm -rf /"));
    }

    #[test]
    fn validate_repo_dest_blocks_escape() {
        let dir = tempfile::tempdir().unwrap();
        let ws = dir.path().join("workspace");
        std::fs::create_dir_all(&ws).unwrap();
        let escape = dir.path().join("escaped");
        let result = validate_repo_dest(&ws, &escape);
        assert!(result.is_err());
    }

    /// A failed pull must clean up the workspace dir it created — no
    /// empty/partial dest may survive to masquerade as a checkout later.
    #[test]
    fn failed_pull_cleans_up_created_dest() {
        let dir = tempfile::tempdir().unwrap();
        let ws = dir.path().join("workspace");
        let url = "/definitely/missing/nw-f14-repo.git";
        let result = pull_repo(
            &ws,
            url,
            "",
            &PullOptions {
                mode: PullMode::Full,
                sha_policy: ShaPolicy::Head,
                ephemeral: false,
            },
        );
        assert!(result.is_err(), "cloning a missing repo must fail");
        let dest = ws.join(clone_dir_name_from_url(url));
        assert!(
            !dest.exists(),
            "failed pull must not leave its workspace dir behind: {}",
            dest.display()
        );
    }

    /// A failed clone must never wipe a PRE-EXISTING destination directory
    /// that has contents — the pull attempt did not create those files.
    #[test]
    fn failed_pull_preserves_preexisting_nonempty_dest() {
        let dir = tempfile::tempdir().unwrap();
        let ws = dir.path().join("workspace");
        let url = "/definitely/missing/nw-f14-repo.git";
        let dest = ws.join(clone_dir_name_from_url(url));
        std::fs::create_dir_all(&dest).unwrap();
        std::fs::write(dest.join("keep-me.txt"), "user data").unwrap();

        let result = pull_repo(
            &ws,
            url,
            "",
            &PullOptions {
                mode: PullMode::Full,
                sha_policy: ShaPolicy::Head,
                ephemeral: false,
            },
        );
        assert!(result.is_err(), "cloning a missing repo must fail");
        assert_eq!(
            std::fs::read_to_string(dest.join("keep-me.txt")).unwrap(),
            "user data",
            "pre-existing contents must survive a failed clone"
        );
    }
}
