use std::collections::HashSet;

/// Resolve a PowerShell `Import-Module ModuleName` to a file path.
///
/// Searches for `ModuleName.psm1`, `ModuleName.psd1`, or `ModuleName/ModuleName.psm1`.
pub fn resolve_import(
    _from_file: &str,
    specifier: &str,
    known_files: &HashSet<&str>,
) -> Option<String> {
    let candidates = [
        format!("{specifier}.psm1"),
        format!("{specifier}.psd1"),
        format!("{specifier}/{specifier}.psm1"),
    ];

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
    fn resolves_psm1_module() {
        let known = set(&["modules/MyModule.psm1"]);
        let result = resolve_import("scripts/main.ps1", "MyModule", &known);
        assert_eq!(result, Some("modules/MyModule.psm1".to_string()));
    }

    #[test]
    fn resolves_psd1_manifest() {
        let known = set(&["modules/MyModule.psd1"]);
        let result = resolve_import("scripts/main.ps1", "MyModule", &known);
        assert_eq!(result, Some("modules/MyModule.psd1".to_string()));
    }

    #[test]
    fn resolves_nested_module() {
        let known = set(&["modules/MyModule/MyModule.psm1"]);
        let result = resolve_import("scripts/main.ps1", "MyModule", &known);
        assert_eq!(result, Some("modules/MyModule/MyModule.psm1".to_string()));
    }

    #[test]
    fn unknown_module_returns_none() {
        let known = set(&["modules/Other.psm1"]);
        let result = resolve_import("scripts/main.ps1", "Missing", &known);
        assert_eq!(result, None);
    }
}
