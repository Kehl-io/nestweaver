use std::collections::HashSet;

/// Resolve a SystemVerilog `import package::symbol` to a file path.
///
/// Searches for `package.sv` or `package.svh`.
/// Wildcard imports (`package::*`) are resolved to the package file.
pub fn resolve_import(
    _from_file: &str,
    specifier: &str,
    known_files: &HashSet<&str>,
) -> Option<String> {
    // The specifier is the package name (parser strips ::symbol or ::*)
    let extensions = ["sv", "svh"];

    let mut best: Option<&str> = None;
    for ext in &extensions {
        let candidate = format!("{specifier}.{ext}");
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
    fn resolves_package_sv() {
        let known = set(&["rtl/my_pkg.sv"]);
        let result = resolve_import("rtl/top.sv", "my_pkg", &known);
        assert_eq!(result, Some("rtl/my_pkg.sv".to_string()));
    }

    #[test]
    fn resolves_package_svh() {
        let known = set(&["include/defs.svh"]);
        let result = resolve_import("rtl/top.sv", "defs", &known);
        assert_eq!(result, Some("include/defs.svh".to_string()));
    }

    #[test]
    fn unknown_package_returns_none() {
        let known = set(&["rtl/other.sv"]);
        let result = resolve_import("rtl/top.sv", "missing", &known);
        assert_eq!(result, None);
    }

    #[test]
    fn multiple_matches_picks_shortest() {
        let known = set(&["vendor/rtl/my_pkg.sv", "rtl/my_pkg.sv"]);
        let result = resolve_import("rtl/top.sv", "my_pkg", &known);
        assert_eq!(result, Some("rtl/my_pkg.sv".to_string()));
    }
}
