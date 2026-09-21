//! Marked-section writes for generated agent guides.
//!
//! `generate-guide --output` and `nestweaver setup` (Codex `AGENTS.md`) must
//! never replace a file a person has edited. Generated text lives between a
//! single pair of markers. An update rewrites only that span.

use anyhow::Context as _;
use std::path::Path;

pub const BEGIN_MARKER: &str = "<!-- nestweaver:begin -->";
pub const END_MARKER: &str = "<!-- nestweaver:end -->";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GuideSectionWrite {
    Created,
    Updated,
}

pub fn write_status(path: &Path, wrote: GuideSectionWrite) -> String {
    match wrote {
        GuideSectionWrite::Created => format!("Guide written to {}", path.display()),
        GuideSectionWrite::Updated => {
            format!("Updated NestWeaver section in {}", path.display())
        }
    }
}

/// Create `path` with the generated body inside the markers, or replace only
/// the existing marked span. A file with no single well-formed pair is left
/// unchanged and this returns an error.
pub fn write_marked_section(
    path: &Path,
    generated: &str,
) -> Result<GuideSectionWrite, anyhow::Error> {
    if !path.exists() {
        let text = render_marked(generated)?;
        atomic_write(path, &text)?;
        return Ok(GuideSectionWrite::Created);
    }
    let existing = std::fs::read_to_string(path).with_context(|| {
        format!(
            "read {} before updating the NestWeaver section",
            path.display()
        )
    })?;
    let updated = splice_marked(&existing, generated)
        .with_context(|| format!("{} was left unchanged", path.display()))?;
    atomic_write(path, &updated)?;
    Ok(GuideSectionWrite::Updated)
}

pub fn render_marked(generated: &str) -> Result<String, anyhow::Error> {
    let body = section_body(generated)?;
    Ok(format!("{BEGIN_MARKER}\n{body}{END_MARKER}\n"))
}

pub fn splice_marked(existing: &str, generated: &str) -> Result<String, anyhow::Error> {
    let body = section_body(generated)?;
    let begins: Vec<usize> = existing
        .match_indices(BEGIN_MARKER)
        .map(|(index, _)| index)
        .collect();
    let ends: Vec<usize> = existing
        .match_indices(END_MARKER)
        .map(|(index, _)| index)
        .collect();
    if begins.is_empty() && ends.is_empty() {
        anyhow::bail!(
            "no NestWeaver section markers ({BEGIN_MARKER} … {END_MARKER}). \
             Refusing to overwrite. Add that pair around the generated section, \
             or remove the file to have it created."
        );
    }
    if begins.len() != 1 || ends.len() != 1 {
        anyhow::bail!(
            "expected exactly one {BEGIN_MARKER} and one {END_MARKER}. \
             Refusing to overwrite."
        );
    }
    let begin = begins[0];
    let end = ends[0];
    if end < begin.saturating_add(BEGIN_MARKER.len()) {
        anyhow::bail!("{END_MARKER} appears before {BEGIN_MARKER}. Refusing to overwrite.");
    }
    let after = end + END_MARKER.len();
    let mut out = String::with_capacity(existing.len() + body.len());
    out.push_str(&existing[..begin]);
    out.push_str(BEGIN_MARKER);
    out.push('\n');
    out.push_str(&body);
    out.push_str(END_MARKER);
    out.push_str(&existing[after..]);
    Ok(out)
}

fn section_body(generated: &str) -> Result<String, anyhow::Error> {
    if generated.contains(BEGIN_MARKER) || generated.contains(END_MARKER) {
        anyhow::bail!("generated guide contains a NestWeaver section marker. Refusing to write.");
    }
    let mut body = generated.to_string();
    if !body.ends_with('\n') {
        body.push('\n');
    }
    Ok(body)
}

fn atomic_write(path: &Path, contents: &str) -> Result<(), anyhow::Error> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty());
    let parent = parent.unwrap_or_else(|| Path::new("."));
    let mut tmp_name = path
        .file_name()
        .unwrap_or(std::ffi::OsStr::new("guide"))
        .to_os_string();
    tmp_name.push(".nestweaver-tmp");
    let tmp_path = parent.join(&tmp_name);
    std::fs::write(&tmp_path, contents).with_context(|| format!("write {}", tmp_path.display()))?;
    if let Err(error) = std::fs::rename(&tmp_path, path) {
        let _ = std::fs::remove_file(&tmp_path);
        return Err(error).with_context(|| format!("replace {}", path.display()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn path_in(dir: &tempfile::TempDir, name: &str) -> PathBuf {
        dir.path().join(name)
    }

    #[test]
    fn creates_a_missing_file_inside_markers() {
        let dir = tempfile::tempdir().unwrap();
        let path = path_in(&dir, "AGENTS.md");
        let wrote = write_marked_section(&path, "generated body").unwrap();
        assert_eq!(wrote, GuideSectionWrite::Created);
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            text,
            "<!-- nestweaver:begin -->\ngenerated body\n<!-- nestweaver:end -->\n"
        );
    }

    #[test]
    fn updates_only_the_marked_span() {
        let dir = tempfile::tempdir().unwrap();
        let path = path_in(&dir, "AGENTS.md");
        std::fs::write(
            &path,
            "KEEP-ABOVE\n<!-- nestweaver:begin -->\nOLD\n<!-- nestweaver:end -->\nKEEP-BELOW\n",
        )
        .unwrap();
        let wrote = write_marked_section(&path, "NEW").unwrap();
        assert_eq!(wrote, GuideSectionWrite::Updated);
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            text,
            "KEEP-ABOVE\n<!-- nestweaver:begin -->\nNEW\n<!-- nestweaver:end -->\nKEEP-BELOW\n"
        );
    }

    #[test]
    fn refuses_a_file_with_no_markers_and_leaves_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = path_in(&dir, "AGENTS.md");
        let original = "# Hand written\n\nDo not replace me.\n";
        std::fs::write(&path, original).unwrap();
        let error = write_marked_section(&path, "generated").unwrap_err();
        let message = format!("{error:#}");
        assert!(
            message.contains("no NestWeaver section markers"),
            "{message}"
        );
        assert!(message.contains("left unchanged"), "{message}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
    }

    #[test]
    fn refuses_a_broken_or_duplicated_pair() {
        let dir = tempfile::tempdir().unwrap();
        for original in [
            "<!-- nestweaver:begin -->\nonly begin\n",
            "<!-- nestweaver:end -->\nthen\n<!-- nestweaver:begin -->\n",
            "<!-- nestweaver:begin -->\na\n<!-- nestweaver:end -->\n<!-- nestweaver:begin -->\nb\n<!-- nestweaver:end -->\n",
        ] {
            let path = path_in(&dir, "AGENTS.md");
            std::fs::write(&path, original).unwrap();
            assert!(write_marked_section(&path, "generated").is_err());
            assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
        }
    }
}
