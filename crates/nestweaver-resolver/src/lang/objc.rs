use std::collections::HashSet;

/// Resolve an Objective-C `#import "file.h"` or `#import <Framework/Header.h>`.
///
/// The specifier is the path string (without quotes/brackets).
/// For framework imports like `Framework/Header.h`, tries the full path and the basename.
pub fn resolve_import(
    from_file: &str,
    specifier: &str,
    known_files: &HashSet<&str>,
) -> Option<String> {
    // Try as a relative path from the importing file's directory
    let dir = from_file.rsplit_once('/').map(|(d, _)| d).unwrap_or("");
    if !dir.is_empty() {
        let relative = format!("{dir}/{specifier}");
        if known_files.contains(relative.as_str()) {
            return Some(relative);
        }
    }

    // Try the specifier as-is or as a suffix
    let mut best: Option<&str> = None;
    for &file in known_files {
        if file == specifier || file.ends_with(&format!("/{specifier}")) {
            if best.is_none() || file.len() < best.unwrap().len() {
                best = Some(file);
            }
        }
    }
    if let Some(f) = best {
        return Some(f.to_string());
    }

    // For framework imports like "Framework/Header.h", try just the basename.
    // This may match a different framework's header with the same name — the
    // shortest-path heuristic prefers the most specific (shallowest) match.
    if let Some((_, basename)) = specifier.rsplit_once('/') {
        let mut best: Option<&str> = None;
        for &file in known_files {
            if file == basename || file.ends_with(&format!("/{basename}")) {
                if best.is_none() || file.len() < best.unwrap().len() {
                    best = Some(file);
                }
            }
        }
        if let Some(f) = best {
            return Some(f.to_string());
        }
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
    fn resolves_quoted_import() {
        let known = set(&["src/MyClass.h"]);
        let result = resolve_import("src/main.m", "MyClass.h", &known);
        assert_eq!(result, Some("src/MyClass.h".to_string()));
    }

    #[test]
    fn resolves_framework_import() {
        let known = set(&["include/Foundation/NSObject.h"]);
        let result = resolve_import("src/main.m", "Foundation/NSObject.h", &known);
        assert_eq!(result, Some("include/Foundation/NSObject.h".to_string()));
    }

    #[test]
    fn resolves_framework_basename_fallback() {
        let known = set(&["headers/NSObject.h"]);
        let result = resolve_import("src/main.m", "Foundation/NSObject.h", &known);
        assert_eq!(result, Some("headers/NSObject.h".to_string()));
    }

    #[test]
    fn unknown_import_returns_none() {
        let known = set(&["src/Other.h"]);
        let result = resolve_import("src/main.m", "Missing.h", &known);
        assert_eq!(result, None);
    }
}
