use std::collections::HashSet;

pub fn resolve_import(
    from_file: &str,
    specifier: &str,
    known_files: &HashSet<&str>,
) -> Option<String> {
    let _ = from_file;

    if specifier.ends_with(".*") {
        return None;
    }

    let mut best: Option<&str> = None;
    for ext in &[".kt", ".java"] {
        let candidate = format!("{}{ext}", specifier.replace('.', "/"));
        for &file in known_files {
            if (file == candidate || file.ends_with(&format!("/{candidate}")))
                && (best.is_none() || file.len() < best.unwrap().len())
            {
                best = Some(file);
            }
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
    fn resolves_kotlin_package_import() {
        let known = set(&["src/main/kotlin/com/example/utils/Helper.kt"]);
        let result = resolve_import(
            "src/main/kotlin/com/example/Main.kt",
            "com.example.utils.Helper",
            &known,
        );
        assert_eq!(
            result,
            Some("src/main/kotlin/com/example/utils/Helper.kt".to_string())
        );
    }

    #[test]
    fn wildcard_import_returns_none() {
        let known = set(&["src/com/example/Foo.kt"]);
        let result = resolve_import("src/Main.kt", "com.example.*", &known);
        assert_eq!(result, None);
    }
}
