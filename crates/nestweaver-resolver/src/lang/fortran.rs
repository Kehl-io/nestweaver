use std::collections::HashSet;

/// Resolve a Fortran `use module_name` to a file path.
///
/// Searches for `module_name.f90`, `.f95`, `.f03`, `.F90` (case-insensitive).
/// When multiple files match, returns the shortest path (most specific match).
pub fn resolve_import(
    _from_file: &str,
    specifier: &str,
    known_files: &HashSet<&str>,
) -> Option<String> {
    let base = specifier.to_lowercase();
    let extensions = ["f90", "f95", "f03"];

    let mut best: Option<&str> = None;
    for ext in &extensions {
        let candidate = format!("{base}.{ext}");
        for &file in known_files {
            let file_lower = file.to_lowercase();
            if file_lower == candidate || file_lower.ends_with(&format!("/{candidate}")) {
                if best.is_none() || file.len() < best.unwrap().len() {
                    best = Some(file);
                }
            }
        }
    }

    best.map(|f| f.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set<'a>(files: &[&'a str]) -> HashSet<&'a str> {
        files.iter().copied().collect()
    }

    #[test]
    fn resolves_module_import() {
        let known = set(&["src/my_module.f90"]);
        let result = resolve_import("src/main.f90", "my_module", &known);
        assert_eq!(result, Some("src/my_module.f90".to_string()));
    }

    #[test]
    fn resolves_case_insensitive() {
        let known = set(&["src/MyModule.F90"]);
        let result = resolve_import("src/main.f90", "mymodule", &known);
        assert_eq!(result, Some("src/MyModule.F90".to_string()));
    }

    #[test]
    fn resolves_f95_extension() {
        let known = set(&["lib/utils.f95"]);
        let result = resolve_import("src/main.f90", "utils", &known);
        assert_eq!(result, Some("lib/utils.f95".to_string()));
    }

    #[test]
    fn unknown_module_returns_none() {
        let known = set(&["src/other.f90"]);
        let result = resolve_import("src/main.f90", "missing", &known);
        assert_eq!(result, None);
    }

    #[test]
    fn multiple_matches_picks_shortest() {
        let known = set(&["vendor/lib/utils.f90", "src/utils.f90"]);
        let result = resolve_import("src/main.f90", "utils", &known);
        assert_eq!(result, Some("src/utils.f90".to_string()));
    }
}
