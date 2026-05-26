/// Resolve a Go import path to a file path.
///
/// Go import paths like `"internal/service"` correspond to directory paths.
/// We find `.go` files that live in the matching package directory.
pub fn resolve_import(from_file: &str, specifier: &str, known_files: &[&str]) -> Option<String> {
    let _ = from_file; // Go imports are module-relative, from_file not needed

    // Remove surrounding quotes if any (import specifier may still have them)
    let specifier = specifier.trim_matches('"');

    // Look for .go files whose path contains the package directory
    for &file in known_files {
        if !file.ends_with(".go") {
            continue;
        }
        // The file should be in a directory that matches the specifier
        // e.g., specifier "internal/service" matches "internal/service/server.go"
        let dir = match file.rfind('/') {
            Some(idx) => &file[..idx],
            None => continue,
        };
        if dir == specifier || dir.ends_with(&format!("/{specifier}")) {
            return Some(file.to_string());
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn go_resolves_package_import() {
        let known = [
            "internal/service/server.go",
            "internal/service/client.go",
            "cmd/main.go",
        ];
        let result = resolve_import("cmd/main.go", "internal/service", &known);
        // Should return the first matching .go file in the package directory
        assert!(result.is_some());
        let path = result.unwrap();
        assert!(
            path == "internal/service/server.go" || path == "internal/service/client.go",
            "unexpected: {path}"
        );
    }

    #[test]
    fn go_resolves_package_with_module_prefix() {
        let known = ["github.com/org/repo/internal/service/server.go"];
        let result = resolve_import(
            "github.com/org/repo/cmd/main.go",
            "internal/service",
            &known,
        );
        assert_eq!(
            result,
            Some("github.com/org/repo/internal/service/server.go".to_string())
        );
    }

    #[test]
    fn go_unknown_package_returns_none() {
        let known = ["internal/other/thing.go"];
        let result = resolve_import("cmd/main.go", "internal/service", &known);
        assert_eq!(result, None);
    }
}
