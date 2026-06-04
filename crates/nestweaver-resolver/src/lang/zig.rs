use std::collections::HashSet;

/// Resolve a Zig `@import("module")` to a file path.
///
/// The specifier is the string literal content (e.g. `module` or `module.zig`).
/// If the specifier already ends in `.zig`, use it directly; otherwise append `.zig`.
pub fn resolve_import(
    from_file: &str,
    specifier: &str,
    known_files: &HashSet<&str>,
) -> Option<String> {
    let candidate = if specifier.ends_with(".zig") {
        specifier.to_string()
    } else {
        format!("{specifier}.zig")
    };

    // Try as a relative path from the importing file's directory
    let dir = from_file.rsplit_once('/').map(|(d, _)| d).unwrap_or("");
    if !dir.is_empty() {
        let relative = format!("{dir}/{candidate}");
        if known_files.contains(relative.as_str()) {
            return Some(relative);
        }
    }

    // Try as an absolute/project-root path
    let mut best: Option<&str> = None;
    for &file in known_files {
        if (file == candidate.as_str() || file.ends_with(&format!("/{candidate}")))
            && (best.is_none() || file.len() < best.unwrap().len())
        {
            best = Some(file);
        }
    }
    if let Some(f) = best {
        return Some(f.to_string());
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set<'a>(files: &[&'a str]) -> HashSet<&'a str> {
        files.iter().copied().collect()
    }

    #[test]
    fn resolves_relative_import() {
        let known = set(&["src/utils.zig"]);
        let result = resolve_import("src/main.zig", "utils", &known);
        assert_eq!(result, Some("src/utils.zig".to_string()));
    }

    #[test]
    fn resolves_with_extension() {
        let known = set(&["src/utils.zig"]);
        let result = resolve_import("src/main.zig", "utils.zig", &known);
        assert_eq!(result, Some("src/utils.zig".to_string()));
    }

    #[test]
    fn resolves_absolute_path() {
        let known = set(&["lib/std.zig"]);
        let result = resolve_import("src/main.zig", "std", &known);
        assert_eq!(result, Some("lib/std.zig".to_string()));
    }

    #[test]
    fn unknown_module_returns_none() {
        let known = set(&["src/other.zig"]);
        let result = resolve_import("src/main.zig", "missing", &known);
        assert_eq!(result, None);
    }
}
