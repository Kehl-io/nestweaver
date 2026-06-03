use std::collections::HashSet;

/// Resolve a Groovy import specifier to a file path.
///
/// Converts `com.example.Foo` → `com/example/Foo.groovy`.
/// Wildcard imports (`com.example.*`) return `None`.
pub fn resolve_import(
    _from_file: &str,
    specifier: &str,
    known_files: &HashSet<&str>,
) -> Option<String> {
    // Skip wildcard imports
    if specifier.ends_with(".*") {
        return None;
    }

    let candidate = format!("{}.groovy", specifier.replace('.', "/"));

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
    fn resolves_package_import() {
        let known = set(&["src/main/groovy/com/example/Foo.groovy"]);
        let result = resolve_import("src/Main.groovy", "com.example.Foo", &known);
        assert_eq!(
            result,
            Some("src/main/groovy/com/example/Foo.groovy".to_string())
        );
    }

    #[test]
    fn wildcard_import_returns_none() {
        let known = set(&["com/example/Foo.groovy"]);
        let result = resolve_import("Main.groovy", "com.example.*", &known);
        assert_eq!(result, None);
    }
}
