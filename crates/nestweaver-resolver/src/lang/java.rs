use std::collections::HashSet;

/// Resolve a Java import specifier to a file path.
///
/// Converts a dotted import like `com.example.Foo` → `com/example/Foo.java`
/// and matches against known file paths.
///
/// Wildcard imports like `com.example.*` are explicitly skipped (return `None`).
pub fn resolve_import(
    from_file: &str,
    specifier: &str,
    known_files: &HashSet<&str>,
) -> Option<String> {
    let _ = from_file; // Java imports are absolute, from_file not needed

    // Skip wildcard imports — cannot resolve to a single file
    if specifier.ends_with(".*") {
        return None;
    }

    // Convert dotted package path to file path
    let candidate = format!("{}.java", specifier.replace('.', "/"));

    // Match against known files (may include path prefix like src/main/java/...)
    for &file in known_files {
        if file == candidate || file.ends_with(&format!("/{candidate}")) {
            return Some(file.to_string());
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
    fn java_resolves_package_import() {
        let known = set(&[
            "src/main/java/com/example/Foo.java",
            "src/main/java/com/example/Bar.java",
        ]);
        let result = resolve_import(
            "src/main/java/com/example/Main.java",
            "com.example.Foo",
            &known,
        );
        assert_eq!(
            result,
            Some("src/main/java/com/example/Foo.java".to_string())
        );
    }

    #[test]
    fn java_resolves_root_level_import() {
        let known = set(&["com/example/Foo.java"]);
        let result = resolve_import("com/example/Main.java", "com.example.Foo", &known);
        assert_eq!(result, Some("com/example/Foo.java".to_string()));
    }

    #[test]
    fn java_unknown_import_returns_none() {
        let known = set(&["com/example/Bar.java"]);
        let result = resolve_import("com/example/Main.java", "com.example.Missing", &known);
        assert_eq!(result, None);
    }

    #[test]
    fn java_wildcard_import_returns_none() {
        let known = set(&[
            "src/main/java/com/example/Foo.java",
            "src/main/java/com/example/Bar.java",
        ]);
        let result = resolve_import("com/example/Main.java", "com.example.*", &known);
        assert_eq!(result, None);
    }
}
