//! Reading and replacing configuration files that belong to the USER.
//!
//! ## Why this module exists
//!
//! Every command that "adds NestWeaver to your editor" edits a file it did not
//! author: `.claude/settings.local.json`, `.claude/settings.json`, `.mcp.json`,
//! `.cursor/mcp.json`, `~/.codex/config.toml`. Those files hold the user's API
//! keys, permission grants and model configuration. NestWeaver owns exactly one
//! entry in each of them; everything else is somebody else's, and a command
//! that cannot read a file has learned nothing about what is in it.
//!
//! `nestweaver admin install-hook` proved what happens without that rule. It
//! read `.claude/settings.local.json`, folded ANY parse failure into
//! `Value::Null`, merged its hook into that, and wrote the result. A single
//! `//` comment — which Claude Code itself accepts in these files — was enough:
//!
//! ```text
//! $ cat .claude/settings.local.json
//! {
//!   // project-local overrides
//!   "env": { "MY_API_KEY": "sk-live-do-not-lose-me" },
//!   "permissions": { "allow": ["Bash(git status:*)"] }
//! }
//! $ nestweaver admin install-hook
//! Hook installed (idempotent) to .claude/settings.local.json    # exit 0
//! $ cat .claude/settings.local.json
//! { "hooks": { "PreToolUse": [ ... ] } }                        # key gone
//! ```
//!
//! Three separate mechanisms, each fixed here:
//!
//! 1. **`Null` on parse failure.** "I cannot read this" is not "there is
//!    nothing here". [`read_json_config`] returns an error.
//! 2. **`fs::write` is not atomic.** It opens the target and truncates it, so
//!    the user's settings are destroyed BEFORE the replacement is written and a
//!    crash in between leaves nothing. [`replace_file_atomically`] writes a
//!    temp file in the same directory, fsyncs it, and renames.
//! 3. **`Path::exists` follows symlinks.** A symlinked settings file was
//!    written through to whatever it pointed at, outside the project.
//!    [`probe`] uses `symlink_metadata` and reports the link as a link.
//!
//! ## The JSONC decision, stated honestly
//!
//! Claude Code and VS Code both accept `//` and `/* */` comments in these
//! files, so a strict JSON parser refuses documents that are legitimately valid
//! for the tool being configured. This module therefore READS JSONC — comments
//! are blanked for parsing — but REFUSES TO REWRITE a file that has them.
//!
//! That asymmetry is the point. `serde_json` cannot round-trip a comment; the
//! only way to "support" JSONC in a serialize-and-replace writer is to drop the
//! user's comments on the floor, which is the same defect wearing a different
//! hat. Reading is safe and makes `--dry-run` work on a commented file, so the
//! refusal can hand the caller something it can actually do. Writing is not, so
//! it does not happen.
//!
//! The comment blanker preserves byte offsets — every comment byte becomes a
//! space, every newline is kept — so a parse position reported against the
//! stripped text names the same line and column in the file on disk.

use std::path::Path;

use anyhow::Context;
use serde_json::Value;

/// What [`probe`] found at a path, WITHOUT following symlinks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileProbe {
    /// The path resolves to something. A dangling symlink counts as existing:
    /// it is not "nothing here", and treating it as absent is how a "create the
    /// file" branch ends up creating a file somewhere else entirely.
    pub existed: bool,
    /// The path itself is a symbolic link.
    pub is_symlink: bool,
}

/// Classify `path` without following symlinks.
///
/// `Path::exists` answers "is there a readable file at the other end of however
/// many links", which is the wrong question for a writer: the answer is about
/// the target and the write lands on the link.
pub fn probe(path: &Path) -> Result<FileProbe, anyhow::Error> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => Ok(FileProbe {
            existed: true,
            is_symlink: metadata.file_type().is_symlink(),
        }),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(FileProbe {
            existed: false,
            is_symlink: false,
        }),
        Err(error) => Err(anyhow::anyhow!(
            "cannot inspect {}: {error}. NestWeaver changed nothing.",
            path.display()
        )),
    }
}

/// Blank out JSONC comments, preserving every other byte and every newline.
///
/// Comment bytes become spaces rather than being deleted, so the result has the
/// same length and the same line structure as the input. A `serde_json` parse
/// error against the result therefore names a line and column that are correct
/// for the file the user will open.
///
/// Comparing the result to the input is also how comments are DETECTED: they
/// are present exactly when the two differ. That equivalence is deliberate —
/// it means any conservative mistake in this scanner (blanking something it
/// should not have) can only ever cause a refusal to write, never a write.
pub fn strip_jsonc_comments(source: &str) -> String {
    let bytes = source.as_bytes();
    let mut out = String::with_capacity(source.len());
    // Start of the run of bytes to copy through untouched. Always an ASCII
    // boundary — 0, a newline, or the byte after `*/` — so slicing is safe.
    let mut verbatim_from = 0usize;
    let mut i = 0usize;
    let mut in_string = false;

    while i < bytes.len() {
        let byte = bytes[i];
        if in_string {
            // A backslash escapes the next byte, including a quote. Skipping
            // two is what keeps `"a\"// not a comment"` intact.
            if byte == b'\\' {
                i += 2;
                continue;
            }
            if byte == b'"' {
                in_string = false;
            }
            i += 1;
            continue;
        }
        match byte {
            b'"' => {
                in_string = true;
                i += 1;
            }
            b'/' if bytes.get(i + 1) == Some(&b'/') => {
                out.push_str(&source[verbatim_from..i]);
                while i < bytes.len() && bytes[i] != b'\n' {
                    out.push(' ');
                    i += 1;
                }
                verbatim_from = i;
            }
            b'/' if bytes.get(i + 1) == Some(&b'*') => {
                out.push_str(&source[verbatim_from..i]);
                out.push(' ');
                out.push(' ');
                i += 2;
                while let Some(&byte) = bytes.get(i) {
                    if byte == b'*' && bytes.get(i + 1) == Some(&b'/') {
                        out.push(' ');
                        out.push(' ');
                        i += 2;
                        break;
                    }
                    // Newlines survive so the line numbering does not shift.
                    out.push(if byte == b'\n' { '\n' } else { ' ' });
                    i += 1;
                }
                verbatim_from = i;
            }
            _ => i += 1,
        }
    }

    out.push_str(&source[verbatim_from.min(source.len())..]);
    out
}

/// A user-owned JSON (or JSONC) configuration document, read but not yet
/// modified.
#[derive(Debug, Clone)]
pub struct JsonConfig {
    /// The path existed, judged with `symlink_metadata`.
    pub existed: bool,
    /// The path is a symbolic link. Writers must refuse; readers need not.
    pub is_symlink: bool,
    /// The bytes on disk contain JSONC comments, so a serialize-and-replace
    /// write would delete them.
    pub has_comments: bool,
    /// The parsed document. An empty object when the file does not exist.
    pub value: Value,
}

/// Read a user-owned JSON/JSONC config, refusing rather than guessing.
///
/// A missing file is not an error — there is genuinely nothing there, and the
/// caller may create it. Anything else that cannot be turned into a JSON object
/// IS an error, and the error names the file, the parse position, and `remedy`.
///
/// `remedy` is the backtick-quoted command the caller wants the user to re-run
/// once the file is fixed; it is a parameter rather than a constant because the
/// message has to name the command the user actually typed.
pub fn read_json_config(path: &Path, remedy: &str) -> Result<JsonConfig, anyhow::Error> {
    let probe = probe(path)?;
    if !probe.existed {
        return Ok(JsonConfig {
            existed: false,
            is_symlink: false,
            has_comments: false,
            value: serde_json::json!({}),
        });
    }

    // Reading through a symlink is harmless and is what makes `--dry-run`
    // useful on one. Only the WRITE path cares that this is a link.
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("read {}. NestWeaver changed nothing.", path.display()))?;
    let stripped = strip_jsonc_comments(&raw);
    let has_comments = stripped != raw;

    // `serde_json::Error`'s Display already ends in "at line L column C", and
    // the blanker above preserved byte offsets precisely so that position names
    // the same spot in the file the user will open. Restating it here produced
    // "trailing comma at line 3 column 1, at line 3 column 1".
    let value: Value = serde_json::from_str(&stripped).map_err(|error| {
        anyhow::anyhow!(
            "{} is not valid JSON: {error}. NestWeaver changed nothing — \
             overwriting a file it cannot read would discard every setting in \
             it, including any credentials. Fix the syntax at that position, or \
             move the file aside, then run {remedy} again.",
            path.display(),
        )
    })?;

    if !value.is_object() {
        let kind = match &value {
            Value::Array(_) => "an array",
            Value::String(_) => "a string",
            Value::Number(_) => "a number",
            Value::Bool(_) => "a boolean",
            Value::Null => "null",
            Value::Object(_) => "an object",
        };
        anyhow::bail!(
            "{} is valid JSON but not an object (found {kind}). NestWeaver \
             changed nothing: whatever this file is, it is the user's, and \
             replacing it is not this command's call to make. Fix it or move it \
             aside, then run {remedy} again.",
            path.display(),
        );
    }

    Ok(JsonConfig {
        existed: true,
        is_symlink: probe.is_symlink,
        has_comments,
        value,
    })
}

/// Refuse to write through a symbolic link, naming where it points.
///
/// `remedy` completes the message with what the caller should do instead.
pub fn refuse_symlink(path: &Path, remedy: &str) -> anyhow::Error {
    let target = std::fs::read_link(path)
        .map(|target| target.display().to_string())
        .unwrap_or_else(|_| "somewhere outside this directory".to_string());
    anyhow::anyhow!(
        "{} is a symbolic link to {target}. NestWeaver changed nothing: writing \
         through the link would modify a file this command was never pointed \
         at, and replacing the link with a regular file would discard the link \
         itself. {remedy}",
        path.display(),
    )
}

/// Refuse to rewrite a file whose comments the writer cannot carry.
///
/// `remedy` completes the message with what the caller should do instead.
pub fn refuse_comments(path: &Path, remedy: &str) -> anyhow::Error {
    anyhow::anyhow!(
        "{} contains JSON comments. NestWeaver changed nothing: it rewrites \
         this file with a JSON serializer, which cannot carry comments, so \
         saving it would silently delete every comment in it. {remedy}",
        path.display(),
    )
}

/// Replace `path` with `contents`, atomically and durably.
///
/// Delegates to [`nestweaver_store::durable_sidecar::atomic_replace_file`], the
/// primitive this repository already uses for every sidecar it cannot afford to
/// half-write: temp file in the same directory, existing permissions carried
/// over, fsync, rename, parent fsync. The user's settings file earns the same
/// treatment for the same reason — a crash must not be able to leave it
/// truncated.
///
/// The caller is responsible for having refused a symlinked `path` first: a
/// rename REPLACES the link rather than following it, which is safe for the
/// link's target but silently destroys the link.
pub fn replace_file_atomically(path: &Path, contents: &str) -> Result<(), anyhow::Error> {
    use std::io::Write;

    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create directory {}", parent.display()))?;
    }
    nestweaver_store::durable_sidecar::atomic_replace_file(path, |file| {
        file.write_all(contents.as_bytes())
    })
    .with_context(|| format!("write {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_comment_is_blanked_and_offsets_are_preserved() {
        let source = "{\n  // project-local overrides\n  \"a\": 1\n}\n";
        let stripped = strip_jsonc_comments(source);
        assert_eq!(
            stripped.len(),
            source.len(),
            "byte offsets must survive so parse positions stay honest"
        );
        assert_eq!(stripped.lines().count(), source.lines().count());
        assert!(!stripped.contains("project-local"));
        let value: Value = serde_json::from_str(&stripped).unwrap();
        assert_eq!(value["a"], 1);
    }

    #[test]
    fn block_comment_keeps_its_newlines() {
        let source = "{\n/* one\n   two */\n  \"a\": 1\n}";
        let stripped = strip_jsonc_comments(source);
        assert_eq!(stripped.len(), source.len());
        assert_eq!(stripped.matches('\n').count(), source.matches('\n').count());
        let value: Value = serde_json::from_str(&stripped).unwrap();
        assert_eq!(value["a"], 1);
    }

    #[test]
    fn slashes_inside_strings_are_not_comments() {
        // The whole file is a legal JSON document that merely LOOKS commented.
        // Blanking any of this would corrupt a value NestWeaver does not own.
        let source = r#"{"url": "https://example.com/a//b", "glob": "/* not a comment */"}"#;
        let stripped = strip_jsonc_comments(source);
        assert_eq!(stripped, source, "no comments here, so nothing to blank");
        let value: Value = serde_json::from_str(&stripped).unwrap();
        assert_eq!(value["url"], "https://example.com/a//b");
    }

    #[test]
    fn escaped_quote_does_not_end_the_string() {
        let source = r#"{"a": "x\"// still a string", "b": 2}"#;
        assert_eq!(strip_jsonc_comments(source), source);
    }

    #[test]
    fn multibyte_comment_text_leaves_valid_utf8() {
        let source = "{\n  // \u{e9}t\u{e9} \u{1f600}\n  \"a\": 1\n}";
        let stripped = strip_jsonc_comments(source);
        assert_eq!(stripped.len(), source.len());
        let value: Value = serde_json::from_str(&stripped).unwrap();
        assert_eq!(value["a"], 1);
    }

    #[test]
    fn absent_file_is_not_an_error_and_is_not_a_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let config = read_json_config(&dir.path().join("nope.json"), "`x`").unwrap();
        assert!(!config.existed);
        assert!(!config.is_symlink);
        assert_eq!(config.value, serde_json::json!({}));
    }

    #[test]
    fn unparseable_file_is_refused_and_names_the_position() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        std::fs::write(&path, "{ \"a\": 1,, }").unwrap();
        let error = read_json_config(&path, "`nestweaver setup`").unwrap_err();
        let message = format!("{error:#}");
        assert!(message.contains("settings.json"), "{message}");
        assert!(message.contains("line 1 column"), "{message}");
        assert!(message.contains("changed nothing"), "{message}");
        assert!(message.contains("`nestweaver setup`"), "{message}");
    }

    #[test]
    fn commented_file_parses_and_is_flagged() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        std::fs::write(&path, "{\n  // hi\n  \"env\": {\"K\": \"v\"}\n}\n").unwrap();
        let config = read_json_config(&path, "`x`").unwrap();
        assert!(config.has_comments);
        assert_eq!(config.value["env"]["K"], "v");
    }

    #[test]
    fn a_json_array_is_refused_rather_than_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        std::fs::write(&path, "[1, 2]").unwrap();
        let error = read_json_config(&path, "`x`").unwrap_err();
        assert!(format!("{error:#}").contains("an array"));
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_is_reported_as_a_symlink_not_as_its_target() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("real.json");
        let link = dir.path().join("link.json");
        std::fs::write(&target, "{\"a\": 1}").unwrap();
        std::os::unix::fs::symlink(&target, &link).unwrap();

        let config = read_json_config(&link, "`x`").unwrap();
        assert!(config.is_symlink, "symlink_metadata must see the link");
        assert_eq!(config.value["a"], 1, "reading through it is still fine");
    }

    #[cfg(unix)]
    #[test]
    fn a_dangling_symlink_counts_as_existing() {
        // `Path::exists` says false here, which would send a caller down its
        // "create the file" branch — and `fs::write` on a dangling link creates
        // the TARGET, outside the directory the command was pointed at.
        let dir = tempfile::tempdir().unwrap();
        let link = dir.path().join("link.json");
        std::os::unix::fs::symlink(dir.path().join("gone.json"), &link).unwrap();
        assert!(!link.exists(), "the premise: exists() follows the link");
        let probe = probe(&link).unwrap();
        assert!(probe.existed);
        assert!(probe.is_symlink);
    }

    #[test]
    fn atomic_replace_does_not_modify_the_original_in_place() {
        // A hard link is a second name for the SAME inode. `fs::write` opens
        // and truncates that inode, so the witness would change too — and a
        // crash between the truncate and the write would leave the user with
        // nothing. A temp-file-plus-rename leaves the old inode untouched.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        let witness = dir.path().join("witness.json");
        std::fs::write(&path, "{\"secret\": \"keep-me\"}").unwrap();
        std::fs::hard_link(&path, &witness).unwrap();

        replace_file_atomically(&path, "{\"replaced\": true}").unwrap();

        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "{\"replaced\": true}"
        );
        assert_eq!(
            std::fs::read_to_string(&witness).unwrap(),
            "{\"secret\": \"keep-me\"}",
            "the original inode must be intact — that is what makes a crash \
             mid-write survivable"
        );
    }

    #[test]
    fn atomic_replace_leaves_no_temp_files_behind() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        replace_file_atomically(&path, "{}").unwrap();
        let entries: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(
            entries.len(),
            1,
            "only the target should remain: {entries:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn atomic_replace_carries_the_existing_permissions_over() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        std::fs::write(&path, "{}").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();

        replace_file_atomically(&path, "{\"a\": 1}").unwrap();

        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            mode, 0o600,
            "a settings file the user locked down must not be widened by a rewrite"
        );
    }
}
