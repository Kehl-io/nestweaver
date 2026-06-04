use std::collections::HashSet;

use crate::util::parent_dir;

// NOTE: Ruby `require`/`require_relative` are parsed as Call references,
// not Import references. Import resolution for Ruby currently relies on
// same-package-fallback. A future enhancement could post-process Call
// references named "require"/"require_relative" to extract import paths.
pub fn resolve_import(
    from_file: &str,
    specifier: &str,
    known_files: &HashSet<&str>,
) -> Option<String> {
    let specifier = specifier.trim_matches(|c| c == '"' || c == '\'');

    // require_relative: resolve relative to current file
    if specifier.starts_with("./") || specifier.starts_with("../") {
        let base_dir = parent_dir(from_file);
        let joined = if base_dir.is_empty() {
            specifier.to_string()
        } else {
            format!("{base_dir}/{specifier}")
        };
        let normalized = normalize_path(&joined);

        for candidate in &[normalized.clone(), format!("{normalized}.rb")] {
            if known_files.contains(&candidate.as_str()) {
                return Some(candidate.clone());
            }
        }
        return None;
    }

    // require: search lib/ directory
    let candidate = format!("lib/{specifier}.rb");
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

    fn set<'a>(files: &[&'a str]) -> HashSet<&'a str> {
        files.iter().copied().collect()
    }

    #[test]
    fn resolves_require_relative() {
        let known = set(&["app/helper.rb"]);
        let result = resolve_import("app/main.rb", "'./helper'", &known);
        assert_eq!(result, Some("app/helper.rb".to_string()));
    }

    #[test]
    fn resolves_lib_require() {
        let known = set(&["lib/json.rb"]);
        let result = resolve_import("app/main.rb", "'json'", &known);
        assert_eq!(result, Some("lib/json.rb".to_string()));
    }

    #[test]
    fn unknown_gem_returns_none() {
        let known = set(&["app/main.rb"]);
        let result = resolve_import("app/main.rb", "'rails'", &known);
        assert_eq!(result, None);
    }
}
