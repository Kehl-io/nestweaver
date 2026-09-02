//! Canonical validation for file lists that drive edge-dependent analysis.
//!
//! These inputs cross CLI, MCP, daemon-proxy, and direct engine boundaries.
//! Keeping validation here prevents a transport from silently dropping a bad
//! element and then reporting a green result for the smaller list.

use std::path::{Component, Path, PathBuf};

use anyhow::{Result, bail};

/// Maximum number of paths accepted by any public changed-file analysis.
pub const MAX_CHANGED_FILES: usize = 1000;
/// Maximum UTF-8 byte length of one repository-relative changed-file path.
pub const MAX_CHANGED_FILE_LEN: usize = 512;

/// Validate and trim changed-file entries while permitting a genuinely empty
/// diff (used by `affected-tests --base-ref HEAD`).
pub fn validate_changed_file_entries(changed_files: &[String]) -> Result<Vec<String>> {
    if changed_files.len() > MAX_CHANGED_FILES {
        bail!(
            "'changed_files' contains {} entries; maximum is {MAX_CHANGED_FILES}",
            changed_files.len()
        );
    }
    changed_files
        .iter()
        .enumerate()
        .map(|(index, raw)| validate_changed_file(raw, index))
        .collect()
}

/// Validate a user-supplied list that must contain at least one changed file.
pub fn require_changed_files(changed_files: &[String]) -> Result<Vec<String>> {
    if changed_files.is_empty() {
        bail!("'changed_files' must contain at least one repository-relative path");
    }
    validate_changed_file_entries(changed_files)
}

/// Path-buffer twin used by blast-radius's public engine API.
pub fn require_changed_paths(changed_files: &[PathBuf]) -> Result<Vec<PathBuf>> {
    if changed_files.is_empty() {
        bail!("'changed_files' must contain at least one repository-relative path");
    }
    if changed_files.len() > MAX_CHANGED_FILES {
        bail!(
            "'changed_files' contains {} entries; maximum is {MAX_CHANGED_FILES}",
            changed_files.len()
        );
    }
    let encoded: Vec<String> = changed_files
        .iter()
        .enumerate()
        .map(|(index, path)| {
            path.to_str().map(str::to_string).ok_or_else(|| {
                anyhow::anyhow!(
                    "invalid changed_files[{index}] {path:?}: paths must be valid UTF-8"
                )
            })
        })
        .collect::<Result<_>>()?;
    Ok(require_changed_files(&encoded)?
        .into_iter()
        .map(PathBuf::from)
        .collect())
}

fn validate_changed_file(raw: &str, index: usize) -> Result<String> {
    let value = raw.trim();
    if value.is_empty() {
        bail!("invalid changed_files[{index}] {raw:?}: path must not be blank or whitespace-only");
    }
    if value.len() > MAX_CHANGED_FILE_LEN {
        bail!(
            "invalid changed_files[{index}]: path is {} bytes; maximum is {MAX_CHANGED_FILE_LEN}",
            value.len()
        );
    }

    let path = Path::new(value);
    let windows_prefix = value
        .as_bytes()
        .get(0..2)
        .is_some_and(|prefix| prefix[0].is_ascii_alphabetic() && prefix[1] == b':')
        || value.starts_with("\\\\");
    let mut has_file_component = false;
    let mut invalid_component = false;
    for component in path.components() {
        match component {
            Component::Normal(_) => has_file_component = true,
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                invalid_component = true
            }
        }
    }
    if path.is_absolute() || windows_prefix || invalid_component || !has_file_component {
        bail!(
            "invalid changed_files[{index}] {raw:?}: expected a repository-relative file path without '..'"
        );
    }

    Ok(value.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trims_valid_paths_and_preserves_missing_new_files() {
        assert_eq!(
            require_changed_files(&[
                "  src/lib.rs  ".to_string(),
                "new/not-indexed.rs".to_string(),
            ])
            .unwrap(),
            vec!["src/lib.rs", "new/not-indexed.rs"]
        );
    }

    #[test]
    fn rejects_empty_blank_mixed_absolute_and_parent_paths_as_a_whole() {
        assert!(require_changed_files(&[]).is_err());
        for (files, invalid_index) in [
            (vec!["".to_string()], 0),
            (vec![" \t\n".to_string()], 0),
            (vec!["src/lib.rs".to_string(), "  ".to_string()], 1),
            (vec!["/tmp/lib.rs".to_string()], 0),
            (vec!["../outside.rs".to_string()], 0),
            (vec!["src/../outside.rs".to_string()], 0),
            (vec!["C:\\tmp\\lib.rs".to_string()], 0),
        ] {
            let error = require_changed_files(&files).unwrap_err().to_string();
            assert!(
                error.contains(&format!("changed_files[{invalid_index}]")),
                "the invalid element must be named: {error}"
            );
        }
    }

    #[test]
    fn an_empty_derived_diff_can_be_validated_without_becoming_user_input() {
        assert_eq!(
            validate_changed_file_entries(&Vec::<String>::new()).unwrap(),
            Vec::<String>::new()
        );
    }

    #[test]
    fn rejects_oversized_lists_and_overlong_paths_at_the_shared_boundary() {
        let too_many = vec!["src/lib.rs".to_string(); MAX_CHANGED_FILES + 1];
        let error = validate_changed_file_entries(&too_many)
            .unwrap_err()
            .to_string();
        assert!(error.contains("maximum is 1000"), "{error}");
        let too_many_paths = vec![PathBuf::from("src/lib.rs"); MAX_CHANGED_FILES + 1];
        let error = require_changed_paths(&too_many_paths)
            .unwrap_err()
            .to_string();
        assert!(error.contains("maximum is 1000"), "{error}");

        let overlong = format!("src/{}", "a".repeat(MAX_CHANGED_FILE_LEN));
        let error = require_changed_files(&["src/ok.rs".to_string(), overlong])
            .unwrap_err()
            .to_string();
        assert!(error.contains("changed_files[1]"), "{error}");
        assert!(error.contains("maximum is 512"), "{error}");
    }
}
