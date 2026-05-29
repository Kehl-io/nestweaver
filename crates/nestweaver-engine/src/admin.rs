//! Admin: instruction store + runtime hook installation (Feature F14).
//!
//! ## Honest framing
//!
//! Injected guidance **helps but is NOT enforcement.** A runtime hook can put
//! text in front of a subagent, but whether the subagent *acts* on that text
//! is probabilistic, not guaranteed (Geng et al. 2025, "Control Illusion").
//! Treat these instructions as a nudge that raises the odds of correct
//! behavior, not as a control mechanism.
//!
//! ## Runtime specificity
//!
//! The hook JSON schemas produced here are **Claude-Code-specific** (the
//! `hooks` / `PreToolUse` / `matcher` shape in `settings.local.json`). Other
//! runtimes are stubbed out behind [`Runtime`] so support can be added later
//! without changing call sites.

use std::path::{Path, PathBuf};

use serde_json::{Value, json};

/// Bundled default for the main (top-level agent) instruction store.
pub const DEFAULT_MAIN_INSTRUCTIONS: &str = r#"# NestWeaver agent instructions

This workspace is indexed by NestWeaver. Prefer its MCP tools for structural
questions over ad-hoc grepping:

- Use `brain_context` for "how does X fit together" questions.
- Use `brain_impact` before modifying a symbol with many callers.
- Use `project_context <slug>` for project-state questions.

These instructions are guidance, not enforcement — follow them when they apply.
"#;

/// Bundled default for the subagent instruction store. Kept short on purpose:
/// it is injected into a subagent's context where attention is scarce.
pub const DEFAULT_SUBAGENT_INSTRUCTIONS: &str = r#"# Subagent guidance (NestWeaver)

You are a subagent in a NestWeaver-indexed workspace. Before answering:

- If your answer references a file path, read that file first.
- For any "every X" / "all Y" question, run a regex/grep sweep before answering.
- For project-state questions, prefer `project_context <slug>` over `brain_search`.
- For URL-bearing messages, fetch the URL before answering.

This guidance helps but is not enforced.
"#;

/// Supported hook runtimes. Only `Claude` is implemented for v1; the enum
/// exists so other runtimes can be slotted in without changing the CLI shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Runtime {
    Claude,
}

impl Runtime {
    pub fn parse(s: &str) -> Result<Self, anyhow::Error> {
        match s.to_ascii_lowercase().as_str() {
            "claude" | "claude-code" => Ok(Runtime::Claude),
            other => anyhow::bail!(
                "unsupported runtime '{other}'; only 'claude' is supported in this version"
            ),
        }
    }
}

/// Resolve the NestWeaver config home (`~/.nestweaver` by default).
fn config_home() -> Result<PathBuf, anyhow::Error> {
    let home = dirs_home()?;
    Ok(home.join(".nestweaver"))
}

/// Minimal home-dir resolution without pulling in an extra crate.
fn dirs_home() -> Result<PathBuf, anyhow::Error> {
    if let Ok(h) = std::env::var("HOME")
        && !h.is_empty()
    {
        return Ok(PathBuf::from(h));
    }
    if let Ok(h) = std::env::var("USERPROFILE")
        && !h.is_empty()
    {
        return Ok(PathBuf::from(h));
    }
    anyhow::bail!("could not determine home directory (HOME/USERPROFILE unset)")
}

/// Path to the main instruction store (`~/.nestweaver/instructions.md`).
pub fn main_instructions_path() -> Result<PathBuf, anyhow::Error> {
    Ok(config_home()?.join("instructions.md"))
}

/// Path to the subagent instruction store
/// (`~/.nestweaver/instructions.subagent.md`).
pub fn subagent_instructions_path() -> Result<PathBuf, anyhow::Error> {
    Ok(config_home()?.join("instructions.subagent.md"))
}

/// Read the main instructions, falling back to the bundled default if the
/// store does not exist.
pub fn read_main_instructions() -> Result<String, anyhow::Error> {
    let path = main_instructions_path()?;
    if path.exists() {
        Ok(std::fs::read_to_string(path)?)
    } else {
        Ok(DEFAULT_MAIN_INSTRUCTIONS.to_string())
    }
}

/// Read the subagent instructions, falling back to the bundled default.
pub fn read_subagent_instructions() -> Result<String, anyhow::Error> {
    let path = subagent_instructions_path()?;
    if path.exists() {
        Ok(std::fs::read_to_string(path)?)
    } else {
        Ok(DEFAULT_SUBAGENT_INSTRUCTIONS.to_string())
    }
}

/// Install a file as the main instruction store.
pub fn set_main_instructions(src: &Path) -> Result<PathBuf, anyhow::Error> {
    let contents = std::fs::read_to_string(src)?;
    let dst = main_instructions_path()?;
    if let Some(parent) = dst.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&dst, contents)?;
    Ok(dst)
}

/// Install a file as the subagent instruction store.
pub fn set_subagent_instructions(src: &Path) -> Result<PathBuf, anyhow::Error> {
    let contents = std::fs::read_to_string(src)?;
    let dst = subagent_instructions_path()?;
    if let Some(parent) = dst.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&dst, contents)?;
    Ok(dst)
}

/// Reset both stores to the bundled defaults (writes them to disk).
pub fn reset_instructions() -> Result<(), anyhow::Error> {
    let home = config_home()?;
    std::fs::create_dir_all(&home)?;
    std::fs::write(home.join("instructions.md"), DEFAULT_MAIN_INSTRUCTIONS)?;
    std::fs::write(
        home.join("instructions.subagent.md"),
        DEFAULT_SUBAGENT_INSTRUCTIONS,
    )?;
    Ok(())
}

/// The command a runtime hook invokes to fetch subagent guidance.
pub const HOOK_COMMAND: &str = "nestweaver admin instructions --for-subagent";

/// Build the PreToolUse hook entry (Claude-Code shape) for the `Task` matcher.
///
/// Shape:
/// ```json
/// {
///   "matcher": "Task",
///   "hooks": [{ "type": "command", "command": "nestweaver admin instructions --for-subagent" }]
/// }
/// ```
fn claude_task_hook_entry() -> Value {
    json!({
        "matcher": "Task",
        "hooks": [
            { "type": "command", "command": HOOK_COMMAND }
        ]
    })
}

/// Compute the JSON patch (the full desired `hooks.PreToolUse` array) to add a
/// `Task`-matcher PreToolUse hook to an existing settings document.
///
/// `existing` is the current settings JSON (e.g. parsed
/// `.claude/settings.local.json`); pass `Value::Null` or an empty object if
/// there is none. The returned value is the settings document with the hook
/// merged in idempotently — if a `Task` matcher already exists it is left
/// untouched (no duplicates).
pub fn compute_claude_hook_patch(existing: &Value) -> Value {
    let mut settings = match existing {
        Value::Object(_) => existing.clone(),
        _ => json!({}),
    };

    if let Some(obj) = settings.as_object_mut() {
        let hooks = obj
            .entry("hooks")
            .or_insert_with(|| json!({}))
            .as_object_mut();
        if let Some(hooks) = hooks {
            let pre = hooks
                .entry("PreToolUse")
                .or_insert_with(|| json!([]))
                .as_array_mut();
            if let Some(arr) = pre {
                let already = arr
                    .iter()
                    .any(|entry| entry.get("matcher").and_then(|m| m.as_str()) == Some("Task"));
                if !already {
                    arr.push(claude_task_hook_entry());
                }
            }
        }
    }

    settings
}

/// Compute a hook patch for the given runtime.
pub fn compute_hook_patch(runtime: Runtime, existing: &Value) -> Result<Value, anyhow::Error> {
    match runtime {
        Runtime::Claude => Ok(compute_claude_hook_patch(existing)),
    }
}

/// Default settings file path for a runtime, relative to the current repo.
pub fn runtime_settings_path(runtime: Runtime) -> PathBuf {
    match runtime {
        Runtime::Claude => PathBuf::from(".claude/settings.local.json"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subagent_default_is_non_empty() {
        // No store on disk in test env → falls back to bundled default.
        let text = read_subagent_instructions().unwrap();
        assert!(!text.trim().is_empty());
        assert!(text.contains("Subagent"));
    }

    #[test]
    fn hook_patch_contains_task_matcher_and_command() {
        let patch = compute_claude_hook_patch(&json!({}));
        let arr = patch["hooks"]["PreToolUse"]
            .as_array()
            .expect("PreToolUse array");
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["matcher"], "Task");
        let cmd = arr[0]["hooks"][0]["command"].as_str().unwrap();
        assert!(cmd.contains("admin instructions --for-subagent"));
    }

    #[test]
    fn hook_patch_is_idempotent() {
        let once = compute_claude_hook_patch(&json!({}));
        let twice = compute_claude_hook_patch(&once);
        let arr = twice["hooks"]["PreToolUse"].as_array().unwrap();
        assert_eq!(
            arr.len(),
            1,
            "applying twice must not duplicate the matcher"
        );
    }

    #[test]
    fn hook_patch_preserves_existing_settings() {
        let existing = json!({
            "permissions": { "allow": ["Bash"] },
            "hooks": { "PreToolUse": [ { "matcher": "Edit", "hooks": [] } ] }
        });
        let patch = compute_claude_hook_patch(&existing);
        assert_eq!(patch["permissions"]["allow"][0], "Bash");
        let arr = patch["hooks"]["PreToolUse"].as_array().unwrap();
        assert_eq!(arr.len(), 2, "existing Edit matcher kept, Task added");
    }

    #[test]
    fn runtime_parse_rejects_unknown() {
        assert!(Runtime::parse("claude").is_ok());
        assert!(Runtime::parse("cursor").is_err());
    }
}
