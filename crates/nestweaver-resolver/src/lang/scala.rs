use std::collections::HashSet;

/// Resolve a Scala import specifier to a file path.
///
/// Converts `com.example.Foo` → `com/example/Foo.scala`.
/// Wildcard imports (`com.example._`) return `None`.
pub fn resolve_import(
    _from_file: &str,
    specifier: &str,
    known_files: &HashSet<&str>,
) -> Option<String> {
    // Skip wildcard imports
    if specifier.ends_with("._") || specifier.ends_with(".*") {
        return None;
    }

    let candidate = format!("{}.scala", specifier.replace('.', "/"));

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
        let known = set(&["src/main/scala/com/example/Foo.scala"]);
        let result = resolve_import("src/main/scala/Main.scala", "com.example.Foo", &known);
        assert_eq!(
            result,
            Some("src/main/scala/com/example/Foo.scala".to_string())
        );
    }

    #[test]
    fn wildcard_import_returns_none() {
        let known = set(&["src/main/scala/com/example/Foo.scala"]);
        let result = resolve_import("Main.scala", "com.example._", &known);
        assert_eq!(result, None);
    }

    #[test]
    fn unknown_import_returns_none() {
        let known = set(&["com/example/Bar.scala"]);
        let result = resolve_import("Main.scala", "com.example.Missing", &known);
        assert_eq!(result, None);
    }
}
