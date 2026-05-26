pub fn resolve_import(_from_file: &str, _specifier: &str, _known_files: &[&str]) -> Option<String> {
    // Swift uses module-level imports — all files in the same target see each other.
    // Module imports (import Foundation) are system frameworks.
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn swift_imports_return_none() {
        let known = ["Sources/main.swift"];
        let result = resolve_import("Sources/main.swift", "Foundation", &known);
        assert_eq!(result, None);
    }
}
