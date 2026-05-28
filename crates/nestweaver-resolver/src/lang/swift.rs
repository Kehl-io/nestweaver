use std::collections::HashSet;

pub fn resolve_import(
    _from_file: &str,
    _specifier: &str,
    _known_files: &HashSet<&str>,
) -> Option<String> {
    // Swift uses module-level imports — all files in the same target see each other.
    // Module imports (import Foundation) are system frameworks.
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set<'a>(files: &[&'a str]) -> HashSet<&'a str> {
        files.iter().copied().collect()
    }

    #[test]
    fn swift_imports_return_none() {
        let known = set(&["Sources/main.swift"]);
        let result = resolve_import("Sources/main.swift", "Foundation", &known);
        assert_eq!(result, None);
    }
}
