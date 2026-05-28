use std::collections::HashSet;

use crate::util::parent_dir;

pub fn resolve_import(
    from_file: &str,
    specifier: &str,
    known_files: &HashSet<&str>,
) -> Option<String> {
    let specifier = specifier.trim_matches(|c| c == '"' || c == '\'');

    if specifier.starts_with("dart:") || specifier.starts_with("package:") {
        return None;
    }

    let base_dir = parent_dir(from_file);
    let candidate = if base_dir.is_empty() {
        specifier.to_string()
    } else {
        format!("{base_dir}/{specifier}")
    };

    if known_files.contains(&candidate.as_str()) {
        return Some(candidate);
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
        let known = set(&["lib/helper.dart"]);
        let result = resolve_import("lib/main.dart", "'helper.dart'", &known);
        assert_eq!(result, Some("lib/helper.dart".to_string()));
    }

    #[test]
    fn skips_dart_sdk_import() {
        let known = set(&["lib/main.dart"]);
        let result = resolve_import("lib/main.dart", "'dart:core'", &known);
        assert_eq!(result, None);
    }

    #[test]
    fn skips_package_import() {
        let known = set(&["lib/main.dart"]);
        let result = resolve_import("lib/main.dart", "'package:flutter/material.dart'", &known);
        assert_eq!(result, None);
    }
}
