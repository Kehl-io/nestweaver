//! Canonical validation for file lists that drive edge-dependent analysis.
//!
//! These inputs cross CLI, MCP, daemon-proxy, and direct engine boundaries.
//! Keeping validation here prevents a transport from silently dropping a bad
//! element and then reporting a green result for the smaller list.

use std::path::{Component, Path, PathBuf};

use anyhow::{Result, bail};

/// Classification of the input, independent of whether the parser emitted symbols.
/// Build/configuration inputs are not covered by the source call graph.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ChangedFileClass {
    Source,
    ExecutionDependency,
    DocumentationOnly,
    Unknown,
}

pub(crate) fn classify_changed_file(path: &Path) -> ChangedFileClass {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("");
    let extension = path
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or("");
    let execution_path = path.components().any(|part| {
        matches!(
            part.as_os_str().to_str(),
            Some(".github" | ".circleci" | "generated" | "__generated__")
        )
    });
    if execution_path
        || matches!(
            name,
            "Makefile"
                | "GNUmakefile"
                | "makefile"
                | "Dockerfile"
                | "Containerfile"
                | "Jenkinsfile"
                | "Justfile"
                | "justfile"
                | "build.rs"
                | "build.gradle.kts"
                | "settings.gradle.kts"
                | "CMakeLists.txt"
                | "go.mod"
                | "go.sum"
                | "Gemfile"
                | "Rakefile"
                | "setup.py"
                | "conftest.py"
                | "Package.swift"
                | "mix.exs"
                | "meson.build"
                | "SConstruct"
                | "SConscript"
        )
        || name.starts_with("Dockerfile.")
        || name.starts_with(".env")
        || name.contains(".config.")
        || name.contains(".generated.")
        || name.contains(".gen.")
        || name.ends_with(".g.cs")
        || name.ends_with(".pb.go")
        || name.ends_with(".pb.cc")
        || name.ends_with(".pb.h")
        || matches!(
            extension,
            "json"
                | "jsonc"
                | "toml"
                | "yaml"
                | "yml"
                | "xml"
                | "lock"
                | "ini"
                | "cfg"
                | "conf"
                | "mk"
                | "cmake"
                | "gradle"
                | "tf"
                | "hcl"
                | "sh"
                | "bash"
                | "ps1"
                | "psm1"
        )
    {
        ChangedFileClass::ExecutionDependency
    } else if nestweaver_parser::is_markdown(path) {
        ChangedFileClass::DocumentationOnly
    } else if nestweaver_parser::detect_language(path).is_some() {
        ChangedFileClass::Source
    } else {
        ChangedFileClass::Unknown
    }
}

/// Apply the common coverage contract. A successful symbol lookup alone does
/// not establish coverage of execution/configuration inputs or unknown formats.
pub(crate) fn disclose_changed_file(
    path: &Path,
    has_symbols: bool,
    status: &mut crate::blast_radius::AnalysisStatus,
    notifications: &mut Vec<crate::blast_radius::Notification>,
) -> ChangedFileClass {
    use crate::blast_radius::{AnalysisStatus, Notification, NotificationLevel};
    let class = classify_changed_file(path);
    let (descriptor, message) = match class {
        ChangedFileClass::Source if has_symbols => return class,
        ChangedFileClass::DocumentationOnly => {
            notifications.push(Notification {
                level: NotificationLevel::Note,
                descriptor: "docs-only-excluded".into(),
                message: format!("{} is classified as Markdown documentation and excluded from executable impact coverage", path.display()),
            });
            return class;
        }
        ChangedFileClass::Source => (
            "changed-file-no-symbols",
            "has no indexed symbols (new file, stale index, or path drift); its impact was not assessed",
        ),
        ChangedFileClass::ExecutionDependency => (
            "changed-file-unassessed",
            "is an execution/build/configuration dependency whose effects are not covered by the source call graph; review manually and run the full suite",
        ),
        ChangedFileClass::Unknown => (
            "changed-file-unassessed",
            "has an unsupported or unknown file type; its impact was not assessed; review manually and run the full suite",
        ),
    };
    *status = (*status).max(AnalysisStatus::Partial);
    notifications.push(Notification {
        level: NotificationLevel::Warning,
        descriptor: descriptor.into(),
        message: format!("{} {message}", path.display()),
    });
    class
}

/// Maximum number of paths accepted by any public changed-file analysis.
pub const MAX_CHANGED_FILES: usize = 1000;
/// Maximum UTF-8 byte length of one repository-relative changed-file path.
pub const MAX_CHANGED_FILE_LEN: usize = 512;

/// Validate and trim changed-file entries while permitting a genuinely empty
/// diff (used by `affected-tests --base-ref HEAD`).
pub fn validate_changed_file_entries(changed_files: &[String]) -> Result<Vec<String>> {
    if changed_files.len() > MAX_CHANGED_FILES {
        bail!(
            "request REJECTED: 'changed_files' contains {} entries; maximum is {MAX_CHANGED_FILES}",
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
            "request REJECTED: 'changed_files' contains {} entries; maximum is {MAX_CHANGED_FILES}",
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
            "request REJECTED: invalid changed_files[{index}]: path is {} bytes; maximum is {MAX_CHANGED_FILE_LEN}",
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
    fn release_change_gate_contract() {
        use crate::blast_radius::{
            AnalysisStatus, GateState, RiskLevel, derive_gate_state, risk_if_unassessed,
        };
        for (file, has_symbols, expected) in [
            ("Makefile", false, ChangedFileClass::ExecutionDependency),
            ("Makefile", true, ChangedFileClass::ExecutionDependency),
            ("Cargo.toml", false, ChangedFileClass::ExecutionDependency),
            (
                ".github/workflows/ci.yml",
                false,
                ChangedFileClass::ExecutionDependency,
            ),
            ("build.rs", true, ChangedFileClass::ExecutionDependency),
            (
                "vite.config.ts",
                true,
                ChangedFileClass::ExecutionDependency,
            ),
            (
                "generated/wiring.rs",
                true,
                ChangedFileClass::ExecutionDependency,
            ),
            (
                "tests/Makefile",
                false,
                ChangedFileClass::ExecutionDependency,
            ),
            ("src/unknown.input", true, ChangedFileClass::Unknown),
            ("src/missing.rs", false, ChangedFileClass::Source),
            ("src/計算.rs", false, ChangedFileClass::Source),
            ("src/huge.rs", false, ChangedFileClass::Source),
        ] {
            let mut status = AnalysisStatus::Complete;
            let mut notes = Vec::new();
            assert_eq!(
                disclose_changed_file(Path::new(file), has_symbols, &mut status, &mut notes),
                expected
            );
            assert_eq!(status, AnalysisStatus::Partial, "{file}");
            let risk = risk_if_unassessed(RiskLevel::Low, &notes);
            assert_eq!(risk, RiskLevel::Unknown, "{file}");
            assert_eq!(
                derive_gate_state(status, risk, false),
                GateState::DegradedUnknown,
                "{file}"
            );
            assert_eq!(risk_if_unassessed(RiskLevel::High, &notes), RiskLevel::High);
        }
        let mut status = AnalysisStatus::Complete;
        let mut notes = Vec::new();
        disclose_changed_file(Path::new("src/lib.rs"), true, &mut status, &mut notes);
        assert_eq!(status, AnalysisStatus::Complete);
        assert!(notes.is_empty());
        disclose_changed_file(Path::new("README.md"), false, &mut status, &mut notes);
        assert_eq!(status, AnalysisStatus::Complete);
        assert_eq!(notes[0].descriptor, "docs-only-excluded");
        disclose_changed_file(Path::new("Makefile"), false, &mut status, &mut notes);
        assert_eq!(
            status,
            AnalysisStatus::Partial,
            "a mixed source/build change cannot be complete"
        );
        status = AnalysisStatus::Degraded;
        disclose_changed_file(Path::new("README.md"), false, &mut status, &mut notes);
        assert_eq!(
            status,
            AnalysisStatus::Degraded,
            "documentation cannot erase stale/resolver failures"
        );
    }

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
        assert!(error.contains("REJECTED"), "{error}");
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
        assert!(error.contains("REJECTED"), "{error}");
        assert!(error.contains("changed_files[1]"), "{error}");
        assert!(error.contains("maximum is 512"), "{error}");
    }
}
