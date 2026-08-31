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

This workspace is indexed by NestWeaver. Use its tools instead of grepping:

| Question | Tool | NOT this |
|---|---|---|
| Find a symbol/note | `brain_search` | `grep`, `rg`, `find` |
| Understand connections | `brain_context` | reading files one by one |
| Read a symbol's source | `read_symbols` | `Read` on the whole file |
| Check blast radius | `brain_impact` or `blast_radius` | manual caller tracing |
| Find text by regex | `regex_search` | `rg`, `grep` |
| Project overview | `project_context <name>` | reading _Overview.md |

For subagents and batch work, prefer CLI (`nestweaver context/search --json`) over MCP — 40-60% cheaper in tokens.

These instructions are guidance, not enforcement — follow them when they apply.
"#;

/// Bundled default for the subagent instruction store. Kept short on purpose:
/// it is injected into a subagent's context where attention is scarce.
pub const DEFAULT_SUBAGENT_INSTRUCTIONS: &str = r#"# Subagent guidance (NestWeaver)

You are a subagent in a NestWeaver-indexed workspace. Use CLI for token efficiency:

- `nestweaver context <seed> --json --db $NESTWEAVER_DB` — structural context (NOT grep)
- `nestweaver search <query> --json --db $NESTWEAVER_DB` — find symbols/notes (NOT grep/rg)
- `nestweaver impact <symbol> --json --db $NESTWEAVER_DB` — blast radius (NOT manual tracing)
- `nestweaver brain search <query> --json --db $NESTWEAVER_DB` — unified note+code search

Before answering:
- If your answer references a file path, read that file first.
- For project-state questions, use `nestweaver project-context <slug> --json`.
- Never run grep/rg/find on an indexed path — use `nestweaver search` or `nestweaver brain search`.
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

/// Compute the *minimal delta* (Claude-Code shape) that `install-hook` would
/// add to the existing settings — i.e. just the `hooks.PreToolUse` entries that
/// are not already present. Unlike [`compute_claude_hook_patch`], this does NOT
/// echo back unrelated pre-existing settings (permissions, other matchers): it
/// is what `--dry-run` should print so the output is the addition, not the
/// whole merged document.
///
/// When the `Task` matcher already exists the `PreToolUse` array is empty
/// (nothing to add).
pub fn compute_claude_hook_delta(existing: &Value) -> Value {
    let already_present = existing
        .get("hooks")
        .and_then(|h| h.get("PreToolUse"))
        .and_then(|p| p.as_array())
        .is_some_and(|arr| {
            arr.iter()
                .any(|entry| entry.get("matcher").and_then(|m| m.as_str()) == Some("Task"))
        });

    let to_add = if already_present {
        json!([])
    } else {
        json!([claude_task_hook_entry()])
    };

    json!({ "hooks": { "PreToolUse": to_add } })
}

/// Compute the minimal dry-run delta for the given runtime.
pub fn compute_hook_delta(runtime: Runtime, existing: &Value) -> Result<Value, anyhow::Error> {
    match runtime {
        Runtime::Claude => Ok(compute_claude_hook_delta(existing)),
    }
}

/// Default settings file path for a runtime, relative to the current repo.
pub fn runtime_settings_path(runtime: Runtime) -> PathBuf {
    match runtime {
        Runtime::Claude => PathBuf::from(".claude/settings.local.json"),
    }
}

/// The command a user runs to see the hook entry without writing anything.
/// Named in every refusal below, so it has to stay a real invocation.
const DRY_RUN_COMMAND: &str = "`nestweaver admin install-hook --dry-run`";

/// The command a user re-runs once they have fixed the settings file.
const INSTALL_COMMAND: &str = "`nestweaver admin install-hook`";

/// Whether the runtime's hook is ALREADY in `existing`.
///
/// Defined as "the dry-run delta adds nothing", so `install-hook` and
/// `install-hook --dry-run` cannot disagree about whether there is work to do.
pub fn hook_already_present(runtime: Runtime, existing: &Value) -> Result<bool, anyhow::Error> {
    let delta = compute_hook_delta(runtime, existing)?;
    Ok(delta
        .get("hooks")
        .and_then(|hooks| hooks.get("PreToolUse"))
        .and_then(|pre| pre.as_array())
        .is_some_and(|entries| entries.is_empty()))
}

/// What [`install_hook`] did. Distinguished so the CLI can say which, instead
/// of the old message's "(idempotent)" hedge that covered both.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookInstall {
    /// The settings file was rewritten with the hook merged in.
    Installed,
    /// The hook was already there. Nothing was written.
    AlreadyPresent,
}

/// Read the runtime settings document that `install-hook` would edit.
///
/// Refuses — rather than substituting an empty document — on anything it cannot
/// parse. This is the whole defect: the previous implementation folded every
/// `serde_json` failure into `Value::Null`, merged its hook into that, and
/// wrote the result, so one `//` comment cost the user their `env` block.
pub fn read_runtime_settings(path: &Path) -> Result<crate::user_config::JsonConfig, anyhow::Error> {
    crate::user_config::read_json_config(path, INSTALL_COMMAND)
}

/// Merge the runtime hook into `path`, preserving every key it does not own.
///
/// NestWeaver owns exactly one entry — the `Task` matcher under
/// `hooks.PreToolUse`. Everything else in the document is the user's, and this
/// function either preserves all of it or writes nothing at all:
///
/// * unparseable, or valid JSON that is not an object → refuse ([`read_runtime_settings`]);
/// * hook already present → return without touching the file, so a second run
///   cannot reformat it, reorder it, or strip anything;
/// * a symbolic link → refuse, because a rename replaces the link and a plain
///   write modifies a file this command was never pointed at;
/// * JSONC comments → refuse, because `serde_json` cannot carry them and
///   "supporting" JSONC in a serialize-and-replace writer means deleting them;
/// * otherwise → merge and replace the file atomically.
pub fn install_hook(runtime: Runtime, path: &Path) -> Result<HookInstall, anyhow::Error> {
    let settings = read_runtime_settings(path)?;

    // Idempotency first, so a repeat run succeeds even on a file this command
    // would refuse to WRITE. There is nothing to do, so there is nothing to
    // refuse.
    if hook_already_present(runtime, &settings.value)? {
        return Ok(HookInstall::AlreadyPresent);
    }

    if settings.is_symlink {
        return Err(crate::user_config::refuse_symlink(
            path,
            &format!(
                "Run {DRY_RUN_COMMAND} to print the hook entry, then add it to the \
                 file the link points at."
            ),
        ));
    }
    if settings.has_comments {
        return Err(crate::user_config::refuse_comments(
            path,
            &format!(
                "Run {DRY_RUN_COMMAND} to print the exact entry to add by hand, or \
                 remove the comments from the file and run {INSTALL_COMMAND} again."
            ),
        ));
    }

    let patched = compute_hook_patch(runtime, &settings.value)?;
    let mut rendered = serde_json::to_string_pretty(&patched)?;
    rendered.push('\n');
    crate::user_config::replace_file_atomically(path, &rendered)?;
    Ok(HookInstall::Installed)
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
    fn dry_run_delta_is_minimal_and_excludes_unrelated_settings() {
        // QA bug C: --dry-run must print only the hook block being added, NOT
        // the entire merged settings document (which may contain alarming
        // pre-existing permissions like Bash(rm -rf ...)).
        let existing = json!({
            "permissions": { "allow": ["Bash(rm -rf /tmp/worktree-cleanup)"] },
            "hooks": { "PreToolUse": [ { "matcher": "Edit", "hooks": [] } ] }
        });
        let delta = compute_claude_hook_delta(&existing);
        let rendered = serde_json::to_string_pretty(&delta).unwrap();

        // The delta carries the PreToolUse Task hook block.
        let arr = delta["hooks"]["PreToolUse"]
            .as_array()
            .expect("delta PreToolUse array");
        assert_eq!(arr.len(), 1, "delta is just the added Task entry");
        assert_eq!(arr[0]["matcher"], "Task");
        assert!(rendered.contains("PreToolUse"));
        assert!(rendered.contains("Task"));

        // It must NOT echo unrelated pre-existing settings.
        assert!(
            delta.get("permissions").is_none(),
            "delta must not include pre-existing permissions"
        );
        assert!(
            !rendered.contains("rm -rf"),
            "delta must not leak the unrelated rm -rf permission"
        );
        // It must NOT echo the pre-existing Edit matcher.
        assert!(
            !rendered.contains("Edit"),
            "delta must not include the pre-existing Edit matcher"
        );
    }

    #[test]
    fn dry_run_delta_empty_when_already_installed() {
        // If the Task matcher is already present, the delta adds nothing.
        let existing = json!({
            "hooks": { "PreToolUse": [ {
                "matcher": "Task",
                "hooks": [ { "type": "command", "command": HOOK_COMMAND } ]
            } ] }
        });
        let delta = compute_claude_hook_delta(&existing);
        let arr = delta["hooks"]["PreToolUse"].as_array().unwrap();
        assert!(
            arr.is_empty(),
            "nothing to add when Task matcher already present"
        );
    }

    #[test]
    fn runtime_parse_rejects_unknown() {
        assert!(Runtime::parse("claude").is_ok());
        assert!(Runtime::parse("cursor").is_err());
    }

    // ── install_hook: never destroy content it did not write ──────────────

    /// The reported reproduction, at the level the CLI calls.
    ///
    /// Before: `serde_json::from_str(&raw).unwrap_or(Value::Null)` turned this
    /// file into `null`, `compute_hook_patch` turned `null` into `{}`, and
    /// `fs::write` put the hook alone on disk — exit 0, "Hook installed".
    #[test]
    fn a_commented_settings_file_keeps_its_secret() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.local.json");
        let original = "{\n  // project-local overrides\n  \
                        \"env\": { \"MY_API_KEY\": \"sk-live-do-not-lose-me\" },\n  \
                        \"permissions\": { \"allow\": [\"Bash(git status:*)\"] }\n}\n";
        std::fs::write(&path, original).unwrap();

        let error = install_hook(Runtime::Claude, &path).unwrap_err();
        let message = format!("{error:#}");

        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            original,
            "a refusal must leave the file byte-for-byte as it was"
        );
        assert!(message.contains("contains JSON comments"), "{message}");
        assert!(
            message.contains("changed nothing"),
            "the message must say what it did, not just what it would not do: {message}"
        );
        assert!(
            message.contains("--dry-run"),
            "a refusal has to hand back something runnable: {message}"
        );
    }

    /// Reading is what makes the refusal's remedy usable, so it has to work on
    /// the very file the write path refuses.
    #[test]
    fn the_dry_run_read_path_works_on_the_file_the_write_path_refuses() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.local.json");
        std::fs::write(&path, "{\n  // hi\n  \"env\": {\"K\": \"v\"}\n}\n").unwrap();

        let settings = read_runtime_settings(&path).unwrap();
        let delta = compute_hook_delta(Runtime::Claude, &settings.value).unwrap();
        assert_eq!(delta["hooks"]["PreToolUse"][0]["matcher"], "Task");
    }

    #[test]
    fn unparseable_settings_are_refused_and_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.local.json");
        let original = "{ \"env\": { \"K\": \"sk-live\" }, }";
        std::fs::write(&path, original).unwrap();

        let error = install_hook(Runtime::Claude, &path).unwrap_err();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
        let message = format!("{error:#}");
        assert!(message.contains("not valid JSON"), "{message}");
        assert!(message.contains("column"), "name the position: {message}");
    }

    #[test]
    fn every_unrelated_key_survives_a_real_install() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.local.json");
        let original = serde_json::json!({
            "env": { "MY_API_KEY": "sk-live-do-not-lose-me" },
            "permissions": { "allow": ["Bash(git status:*)"], "deny": ["Bash(rm:*)"] },
            "model": "opus",
            "hooks": { "PreToolUse": [ { "matcher": "Edit", "hooks": [] } ] },
            "somethingNestWeaverHasNeverHeardOf": { "nested": [1, 2, {"deep": true}] }
        });
        std::fs::write(&path, serde_json::to_string_pretty(&original).unwrap()).unwrap();

        assert_eq!(
            install_hook(Runtime::Claude, &path).unwrap(),
            HookInstall::Installed
        );

        let after: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        for key in [
            "env",
            "permissions",
            "model",
            "somethingNestWeaverHasNeverHeardOf",
        ] {
            assert_eq!(
                after[key], original[key],
                "`{key}` must survive byte-for-byte in value"
            );
        }
        let matchers: Vec<&str> = after["hooks"]["PreToolUse"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|entry| entry["matcher"].as_str())
            .collect();
        assert_eq!(matchers, vec!["Edit", "Task"]);
    }

    #[test]
    fn running_twice_writes_once_and_changes_nothing_the_second_time() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.local.json");
        std::fs::write(&path, "{\"env\": {\"K\": \"v\"}}").unwrap();

        assert_eq!(
            install_hook(Runtime::Claude, &path).unwrap(),
            HookInstall::Installed
        );
        let after_first = std::fs::read_to_string(&path).unwrap();

        assert_eq!(
            install_hook(Runtime::Claude, &path).unwrap(),
            HookInstall::AlreadyPresent,
            "the second run has nothing to add and must say so"
        );
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            after_first,
            "an idempotent command must not rewrite the file at all"
        );

        let after: Value = serde_json::from_str(&after_first).unwrap();
        assert_eq!(after["hooks"]["PreToolUse"].as_array().unwrap().len(), 1);
        assert_eq!(after["env"]["K"], "v");
    }

    /// Idempotency outranks both refusals: if there is nothing to write, a file
    /// this command could not safely write is not a problem.
    #[test]
    fn a_commented_file_that_already_has_the_hook_succeeds_without_writing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.local.json");
        let original = "{\n  // mine\n  \"hooks\": { \"PreToolUse\": [ \
                        { \"matcher\": \"Task\", \"hooks\": [] } ] }\n}\n";
        std::fs::write(&path, original).unwrap();

        assert_eq!(
            install_hook(Runtime::Claude, &path).unwrap(),
            HookInstall::AlreadyPresent
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
    }

    /// The write must not modify the original inode. A hard link is a second
    /// name for it: `fs::write` truncates it in place, so a crash between the
    /// truncate and the write leaves the user with an empty settings file.
    #[test]
    fn the_write_replaces_the_file_rather_than_truncating_it_in_place() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.local.json");
        let witness = dir.path().join("witness.json");
        let original = "{\"env\": {\"MY_API_KEY\": \"sk-live-do-not-lose-me\"}}";
        std::fs::write(&path, original).unwrap();
        std::fs::hard_link(&path, &witness).unwrap();

        install_hook(Runtime::Claude, &path).unwrap();

        assert_eq!(
            std::fs::read_to_string(&witness).unwrap(),
            original,
            "the original inode must still hold the original bytes at every \
             instant of the write"
        );
        let after: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(after["env"]["MY_API_KEY"], "sk-live-do-not-lose-me");

        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(
            leftovers.len(),
            2,
            "no temp file may survive: {leftovers:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_settings_file_is_refused_and_its_target_is_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let outside = dir.path().join("elsewhere.json");
        let link = dir.path().join("settings.local.json");
        let original = "{\"env\": {\"SHARED_KEY\": \"sk-live-shared\"}}";
        std::fs::write(&outside, original).unwrap();
        std::os::unix::fs::symlink(&outside, &link).unwrap();

        let error = install_hook(Runtime::Claude, &link).unwrap_err();
        let message = format!("{error:#}");

        assert_eq!(
            std::fs::read_to_string(&outside).unwrap(),
            original,
            "a file outside the directory the command was pointed at must not \
             be edited by it"
        );
        assert!(
            std::fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink(),
            "the link itself is content NestWeaver did not write"
        );
        assert!(message.contains("symbolic link"), "{message}");
        assert!(message.contains("elsewhere.json"), "name it: {message}");
        assert!(message.contains("--dry-run"), "{message}");
    }

    #[test]
    fn a_missing_settings_file_is_created_with_only_the_hook() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".claude/settings.local.json");
        assert_eq!(
            install_hook(Runtime::Claude, &path).unwrap(),
            HookInstall::Installed
        );
        let after: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(after["hooks"]["PreToolUse"][0]["matcher"], "Task");
    }

    #[test]
    fn presence_and_the_dry_run_delta_cannot_disagree() {
        let empty = json!({});
        assert!(!hook_already_present(Runtime::Claude, &empty).unwrap());
        let installed = compute_hook_patch(Runtime::Claude, &empty).unwrap();
        assert!(hook_already_present(Runtime::Claude, &installed).unwrap());
    }
}
