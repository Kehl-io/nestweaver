use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::lifecycle;

/// Directory holding per-instance launch agents (`~/Library/LaunchAgents`).
fn launchd_agents_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join("Library")
        .join("LaunchAgents")
}

/// True if `db_path` lives under a temporary directory (`/tmp`, `/private/tmp`,
/// `/var/folders`, or `$TMPDIR`). Daemons for temp DBs are ephemeral (tests,
/// throwaway repros) and must never receive a persistent launchd agent — that
/// was the source of the leaked, crash-looping `io.kehl.nestweaver.*` agents.
pub fn is_temp_db_path(db_path: &Path) -> bool {
    let mut bases: Vec<PathBuf> = vec![
        PathBuf::from("/tmp"),
        PathBuf::from("/private/tmp"),
        PathBuf::from("/var/folders"),
        PathBuf::from("/private/var/folders"),
    ];
    if let Some(t) = std::env::var_os("TMPDIR") {
        bases.push(PathBuf::from(t));
    }
    bases.iter().any(|b| db_path.starts_with(b))
}

/// Extract the `--db <path>` value from a nestweaver launchd plist's
/// ProgramArguments. Returns `None` if not present.
pub fn parse_db_path_from_plist(content: &str) -> Option<PathBuf> {
    let mut strings: Vec<&str> = Vec::new();
    let mut rest = content;
    while let Some(start) = rest.find("<string>") {
        let after = &rest[start + "<string>".len()..];
        match after.find("</string>") {
            Some(end) => {
                strings.push(&after[..end]);
                rest = &after[end..];
            }
            None => break,
        }
    }
    let pos = strings.iter().position(|s| *s == "--db")?;
    strings.get(pos + 1).map(|s| PathBuf::from(*s))
}

/// Result of a [`gc_orphaned_agents`] pass.
pub struct GcReport {
    /// Labels whose plist was booted out and deleted.
    pub removed: Vec<String>,
    /// Labels kept (their `--db` path still exists on a real, non-temp path).
    pub kept: Vec<String>,
}

/// Remove orphaned nestweaver launch agents — those whose `--db` path no longer
/// exists or lives under a temp dir — by booting them out and deleting the plist
/// file. Agents whose DB still exists on a real path are kept.
pub fn gc_orphaned_agents() -> Result<GcReport> {
    let dir = launchd_agents_dir();
    let uid = unsafe { libc::getuid() };
    let mut removed = Vec::new();
    let mut kept = Vec::new();

    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(_) => return Ok(GcReport { removed, kept }),
    };

    for entry in entries.flatten() {
        let path = entry.path();
        let Some(fname) = path.file_name().and_then(|f| f.to_str()) else {
            continue;
        };
        // Only per-instance daemon agents: io.kehl.nestweaver.<hash>.plist.
        let Some(label) = fname
            .strip_suffix(".plist")
            .filter(|l| l.starts_with("io.kehl.nestweaver."))
        else {
            continue;
        };

        let content = std::fs::read_to_string(&path).unwrap_or_default();
        let orphaned = match parse_db_path_from_plist(&content) {
            Some(db) => is_temp_db_path(&db) || !db.exists(),
            None => true, // unparseable → treat as orphaned cruft
        };

        if orphaned {
            let _ = Command::new("launchctl")
                .args(["bootout", &format!("gui/{uid}/{label}")])
                .output();
            let _ = std::fs::remove_file(&path);
            removed.push(label.to_string());
        } else {
            kept.push(label.to_string());
        }
    }

    Ok(GcReport { removed, kept })
}

pub fn generate_plist(
    instance_id: &str,
    binary_path: &Path,
    db_path: &Path,
    log_path: &Path,
) -> String {
    let label = lifecycle::launchd_label(instance_id);
    let binary = binary_path.display();
    let db = db_path.display();
    let log = log_path.display();

    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{label}</string>
    <key>ProgramArguments</key>
    <array>
        <string>{binary}</string>
        <string>daemon</string>
        <string>--db</string>
        <string>{db}</string>
        <string>run</string>
    </array>
    <key>StandardOutPath</key>
    <string>{log}</string>
    <key>StandardErrorPath</key>
    <string>{log}</string>
    <key>KeepAlive</key>
    <dict>
        <key>Crashed</key>
        <true/>
    </dict>
    <key>ProcessType</key>
    <string>Interactive</string>
</dict>
</plist>
"#
    )
}

pub fn install_and_start(instance_id: &str, plist_content: &str) -> Result<()> {
    let plist_path = lifecycle::launchd_plist_path(instance_id);
    let label = lifecycle::launchd_label(instance_id);
    let uid = unsafe { libc::getuid() };

    if let Some(parent) = plist_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    std::fs::write(&plist_path, plist_content)
        .with_context(|| format!("write plist: {}", plist_path.display()))?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&plist_path, std::fs::Permissions::from_mode(0o644))?;
    }

    let _ = Command::new("launchctl")
        .args(["enable", &format!("gui/{uid}/{label}")])
        .output();

    let output = Command::new("launchctl")
        .args([
            "bootstrap",
            &format!("gui/{uid}"),
            &plist_path.to_string_lossy(),
        ])
        .output()
        .context("failed to run launchctl bootstrap")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if !stderr.contains("already loaded") && !stderr.contains("already bootstrapped") {
            anyhow::bail!("launchctl bootstrap failed: {stderr}");
        }
    }

    // Kickstart the agent — bootstrap only registers, doesn't start without RunAtLoad
    let kick = Command::new("launchctl")
        .args(["kickstart", &format!("gui/{uid}/{label}")])
        .output()
        .context("failed to run launchctl kickstart")?;

    if !kick.status.success() {
        let stderr = String::from_utf8_lossy(&kick.stderr);
        anyhow::bail!("launchctl kickstart failed: {stderr}");
    }

    Ok(())
}

pub fn stop_and_uninstall(instance_id: &str) -> Result<()> {
    let plist_path = lifecycle::launchd_plist_path(instance_id);
    let label = lifecycle::launchd_label(instance_id);
    let uid = unsafe { libc::getuid() };

    let _ = Command::new("launchctl")
        .args(["bootout", &format!("gui/{uid}/{label}")])
        .output();

    let _ = std::fs::remove_file(&plist_path);

    Ok(())
}

pub fn is_running(instance_id: &str) -> bool {
    let label = lifecycle::launchd_label(instance_id);
    let uid = unsafe { libc::getuid() };

    Command::new("launchctl")
        .args(["print", &format!("gui/{uid}/{label}")])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_db_path_from_plist_extracts_db_arg() {
        let plist = generate_plist(
            "abc123",
            Path::new("/usr/local/bin/nestweaver"),
            Path::new("/Users/k/dev/repo/brain.lbug"),
            Path::new("/tmp/log"),
        );
        assert_eq!(
            parse_db_path_from_plist(&plist),
            Some(PathBuf::from("/Users/k/dev/repo/brain.lbug"))
        );
        assert_eq!(parse_db_path_from_plist("<plist></plist>"), None);
    }

    #[test]
    fn is_temp_db_path_flags_temp_but_not_real_paths() {
        assert!(is_temp_db_path(Path::new("/tmp/ppi2/test.lbug")));
        assert!(is_temp_db_path(Path::new("/private/tmp/x/test.lbug")));
        assert!(is_temp_db_path(Path::new("/var/folders/xx/y/T/test.lbug")));
        assert!(!is_temp_db_path(Path::new(
            "/home/user/.local/share/nestweaver/my-brain/brain.lbug"
        )));
    }
}
