use std::collections::HashSet;

/// Resolve a Pascal `uses unit_name` to a file path.
///
/// Searches for `unit_name.pas` or `unit_name.pp` (case-insensitive).
pub fn resolve_import(
    _from_file: &str,
    specifier: &str,
    known_files: &HashSet<&str>,
) -> Option<String> {
    let base = specifier.to_lowercase();
    let extensions = ["pas", "pp"];

    let mut best: Option<&str> = None;
    for ext in &extensions {
        let candidate = format!("{base}.{ext}");
        for &file in known_files {
            let file_lower = file.to_lowercase();
            if (file_lower == candidate || file_lower.ends_with(&format!("/{candidate}")))
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
    fn resolves_unit_import() {
        let known = set(&["src/MyUnit.pas"]);
        let result = resolve_import("src/main.pas", "myunit", &known);
        assert_eq!(result, Some("src/MyUnit.pas".to_string()));
    }

    #[test]
    fn resolves_pp_extension() {
        let known = set(&["lib/Utils.pp"]);
        let result = resolve_import("src/main.pas", "utils", &known);
        assert_eq!(result, Some("lib/Utils.pp".to_string()));
    }

    #[test]
    fn unknown_unit_returns_none() {
        let known = set(&["src/Other.pas"]);
        let result = resolve_import("src/main.pas", "missing", &known);
        assert_eq!(result, None);
    }

    #[test]
    fn multiple_matches_picks_shortest() {
        let known = set(&["vendor/lib/MyUnit.pas", "src/MyUnit.pas"]);
        let result = resolve_import("src/main.pas", "myunit", &known);
        assert_eq!(result, Some("src/MyUnit.pas".to_string()));
    }
}
