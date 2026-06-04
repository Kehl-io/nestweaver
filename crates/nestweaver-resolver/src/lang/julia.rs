use std::collections::HashSet;

/// Resolve a Julia `using PackageName`, `import PackageName`, or `include("file.jl")`.
///
/// For package imports: searches for `PackageName.jl` and `src/PackageName.jl`.
/// For `include` specifiers (containing `/` or ending in `.jl`): resolves relative to from_file.
pub fn resolve_import(
    from_file: &str,
    specifier: &str,
    known_files: &HashSet<&str>,
) -> Option<String> {
    // include("file.jl") — relative path resolution
    if specifier.contains('/') || specifier.ends_with(".jl") {
        let dir = from_file.rsplit_once('/').map(|(d, _)| d).unwrap_or("");
        if !dir.is_empty() {
            let relative = format!("{dir}/{specifier}");
            if known_files.contains(relative.as_str()) {
                return Some(relative);
            }
        }

        // Also try as-is
        let mut best: Option<&str> = None;
        for &file in known_files {
            if (file == specifier || file.ends_with(&format!("/{specifier}")))
                && (best.is_none() || file.len() < best.unwrap().len())
            {
                best = Some(file);
            }
        }
        if let Some(f) = best {
            return Some(f.to_string());
        }

        return None;
    }

    // Package import: using/import PackageName
    let candidates = [format!("{specifier}.jl"), format!("src/{specifier}.jl")];

    let mut best: Option<&str> = None;
    for candidate in &candidates {
        for &file in known_files {
            if (file == candidate.as_str() || file.ends_with(&format!("/{candidate}")))
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
    fn resolves_package_import() {
        let known = set(&["packages/MyPkg/src/MyPkg.jl"]);
        let result = resolve_import("src/main.jl", "MyPkg", &known);
        assert_eq!(result, Some("packages/MyPkg/src/MyPkg.jl".to_string()));
    }

    #[test]
    fn resolves_root_level_package() {
        let known = set(&["MyPkg.jl"]);
        let result = resolve_import("main.jl", "MyPkg", &known);
        assert_eq!(result, Some("MyPkg.jl".to_string()));
    }

    #[test]
    fn resolves_include_relative() {
        let known = set(&["src/utils.jl"]);
        let result = resolve_import("src/main.jl", "utils.jl", &known);
        assert_eq!(result, Some("src/utils.jl".to_string()));
    }

    #[test]
    fn resolves_include_with_path() {
        let known = set(&["src/lib/helpers.jl"]);
        let result = resolve_import("src/main.jl", "lib/helpers.jl", &known);
        assert_eq!(result, Some("src/lib/helpers.jl".to_string()));
    }

    #[test]
    fn unknown_package_returns_none() {
        let known = set(&["src/Other.jl"]);
        let result = resolve_import("src/main.jl", "Missing", &known);
        assert_eq!(result, None);
    }
}
