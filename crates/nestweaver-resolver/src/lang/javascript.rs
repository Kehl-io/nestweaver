use crate::util::parent_dir;

/// Resolve a JavaScript/TypeScript import specifier to a file path.
///
/// For relative imports (starting with `.` or `..`):
///   - Try exact path
///   - Try with .js, .ts, .tsx, .jsx extensions
///   - Try as directory index: /index.js, /index.ts, /index.tsx, /index.jsx
///
/// Non-relative imports (package imports) return None.
pub fn resolve_import(from_file: &str, specifier: &str, known_files: &[&str]) -> Option<String> {
    if !specifier.starts_with('.') {
        // Package import — cannot resolve without node_modules
        return None;
    }

    let base_dir = parent_dir(from_file);
    let joined = join_path(base_dir, specifier);
    let normalized = normalize_path(&joined);

    // Try exact path
    if known_files.contains(&normalized.as_str()) {
        return Some(normalized);
    }

    // Try with extensions
    for ext in &[".js", ".ts", ".tsx", ".jsx"] {
        let candidate = format!("{normalized}{ext}");
        if known_files.contains(&candidate.as_str()) {
            return Some(candidate);
        }
    }

    // Try as directory index
    for ext in &["/index.js", "/index.ts", "/index.tsx", "/index.jsx"] {
        let candidate = format!("{normalized}{ext}");
        if known_files.contains(&candidate.as_str()) {
            return Some(candidate);
        }
    }

    None
}

fn join_path(base: &str, rel: &str) -> String {
    if base.is_empty() {
        rel.to_string()
    } else {
        format!("{base}/{rel}")
    }
}

/// Normalize path components, resolving `.` and `..` segments.
fn normalize_path(path: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();
    for segment in path.split('/') {
        match segment {
            "." | "" => {}
            ".." => {
                parts.pop();
            }
            s => parts.push(s),
        }
    }
    parts.join("/")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_relative_import_js() {
        let known = ["src/helper.js", "src/index.js"];
        let result = resolve_import("src/main.js", "./helper", &known);
        assert_eq!(result, Some("src/helper.js".to_string()));
    }

    #[test]
    fn resolves_relative_import_ts() {
        let known = ["src/utils.ts"];
        let result = resolve_import("src/main.ts", "./utils", &known);
        assert_eq!(result, Some("src/utils.ts".to_string()));
    }

    #[test]
    fn resolves_barrel_import_js() {
        let known = ["src/utils/index.js", "src/utils/helper.js"];
        let result = resolve_import("src/main.js", "./utils", &known);
        assert_eq!(result, Some("src/utils/index.js".to_string()));
    }

    #[test]
    fn resolves_parent_dir_import() {
        let known = ["src/shared.ts", "src/components/Button.tsx"];
        let result = resolve_import("src/components/Button.tsx", "../shared", &known);
        assert_eq!(result, Some("src/shared.ts".to_string()));
    }

    #[test]
    fn non_relative_import_returns_none() {
        let known = ["node_modules/lodash/index.js"];
        let result = resolve_import("src/main.js", "lodash", &known);
        assert_eq!(result, None);
    }

    #[test]
    fn unknown_file_returns_none() {
        let known: [&str; 0] = [];
        let result = resolve_import("src/main.js", "./missing", &known);
        assert_eq!(result, None);
    }
}
