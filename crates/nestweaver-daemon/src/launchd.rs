use std::path::Path;
use std::process::Command;
use anyhow::{Context, Result};

use crate::lifecycle;

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
