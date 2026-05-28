use std::collections::HashSet;

use crate::util::parent_dir;

pub fn resolve_import(
    from_file: &str,
    specifier: &str,
    known_files: &HashSet<&str>,
) -> Option<String> {
    let specifier = specifier.trim_matches(|c| c == '"' || c == '\'');
    let specifier = specifier.trim_start_matches('\\');

    // PSR-4 style: App\Models\User -> App/Models/User.php
    let candidate = format!("{}.php", specifier.replace('\\', "/"));

    for &file in known_files {
        if file == candidate || file.ends_with(&format!("/{candidate}")) {
            return Some(file.to_string());
        }
    }

    // Try relative include/require paths
    if specifier.ends_with(".php") {
        let base_dir = parent_dir(from_file);
        let joined = if base_dir.is_empty() {
            specifier.to_string()
        } else {
            format!("{base_dir}/{specifier}")
        };
        if known_files.contains(&joined.as_str()) {
            return Some(joined);
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
    fn resolves_psr4_namespace() {
        let known = set(&["src/App/Models/User.php"]);
        let result = resolve_import(
            "src/App/Controllers/UserController.php",
            "App\\Models\\User",
            &known,
        );
        assert_eq!(result, Some("src/App/Models/User.php".to_string()));
    }

    #[test]
    fn unknown_namespace_returns_none() {
        let known = set(&["src/App/Models/User.php"]);
        let result = resolve_import(
            "src/index.php",
            "Illuminate\\Support\\Facades\\Route",
            &known,
        );
        assert_eq!(result, None);
    }
}
