use std::collections::{HashMap, HashSet};

use crate::util::parent_dir;
use crate::workspace::GoModule;

/// Resolve a Go import path to one file of the imported package.
///
/// A Go import names a whole package (a directory), so this is only a
/// representative: the first file of [`GoPackageIndex::package_files`]. The
/// import graph records every file of the package; callers that only ask
/// "does this import name a repository package" can use this.
pub fn resolve_import(
    from_file: &str,
    specifier: &str,
    known_files: &HashSet<&str>,
    modules: &[GoModule],
) -> Option<String> {
    let _ = from_file; // Go imports are module-relative, from_file not needed
    GoPackageIndex::new(known_files.iter().copied())
        .package_files(specifier, modules)
        .first()
        .map(|file| file.to_string())
}

/// Non-test `.go` files grouped by directory: a Go package is a directory.
pub struct GoPackageIndex<'a> {
    dirs: HashMap<&'a str, Vec<&'a str>>,
}

impl<'a> GoPackageIndex<'a> {
    pub fn new(files: impl IntoIterator<Item = &'a str>) -> Self {
        let mut dirs: HashMap<&'a str, Vec<&'a str>> = HashMap::new();
        for file in files {
            // `_test.go` files belong to the package only while testing it;
            // an importer never sees their declarations.
            if !file.ends_with(".go") || file.ends_with("_test.go") {
                continue;
            }
            dirs.entry(parent_dir(file)).or_default().push(file);
        }
        for files in dirs.values_mut() {
            files.sort_unstable();
        }
        Self { dirs }
    }

    /// Every non-test file of the package `specifier` names, sorted.
    ///
    /// With known modules, an import path under a module path maps onto that
    /// module's directory (`example.com/app/internal/calc` under
    /// `module example.com/app` is `internal/calc`); the longest module path
    /// wins, so a nested module beats its parent. Any other path is the
    /// standard library or an external module: it resolves only to a
    /// vendored copy (`vendor/<path>`). Without any known module (no
    /// `go.mod`, GOPATH layout) the path is matched against directories that
    /// equal it or end with it, shortest directory first.
    pub fn package_files(&self, specifier: &str, modules: &[GoModule]) -> Vec<&'a str> {
        let specifier = specifier.trim_matches(|c| c == '"' || c == '`');
        if specifier.is_empty() {
            return Vec::new();
        }
        if !modules.is_empty() {
            let dir = modules
                .iter()
                .filter_map(|module| {
                    let rest = if specifier == module.path {
                        ""
                    } else {
                        specifier
                            .strip_prefix(module.path.as_str())?
                            .strip_prefix('/')?
                    };
                    Some((module.path.len(), join_dir(&module.directory, rest)))
                })
                .max_by_key(|(len, _)| *len)
                .map(|(_, dir)| dir)
                .unwrap_or_else(|| format!("vendor/{specifier}"));
            return self.dirs.get(dir.as_str()).cloned().unwrap_or_default();
        }
        let suffix = format!("/{specifier}");
        self.dirs
            .iter()
            .filter(|(dir, _)| **dir == specifier || dir.ends_with(&suffix))
            .min_by(|(a, _), (b, _)| a.len().cmp(&b.len()).then_with(|| a.cmp(b)))
            .map(|(_, files)| files.clone())
            .unwrap_or_default()
    }
}

/// The package name Go binds for an import whose files declare no package
/// clause we know: the last path element, without a `/vN` major-version
/// element or a `gopkg.in`-style `.vN` suffix.
pub fn default_package_name(specifier: &str) -> &str {
    let specifier = specifier.trim_matches(|c| c == '"' || c == '`');
    let mut segments = specifier.rsplit('/');
    let last = segments.next().unwrap_or(specifier);
    let is_major = |segment: &str| {
        segment
            .strip_prefix('v')
            .is_some_and(|n| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()))
    };
    let last = if is_major(last) {
        segments.next().unwrap_or(last)
    } else {
        last
    };
    match last.rsplit_once('.') {
        Some((name, version)) if is_major(version) => name,
        _ => last,
    }
}

fn join_dir(base: &str, rest: &str) -> String {
    match (base.is_empty(), rest.is_empty()) {
        (true, _) => rest.to_string(),
        (false, true) => base.to_string(),
        (false, false) => format!("{base}/{rest}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set<'a>(files: &[&'a str]) -> HashSet<&'a str> {
        files.iter().copied().collect()
    }

    fn module(path: &str, directory: &str) -> GoModule {
        GoModule {
            path: path.into(),
            directory: directory.into(),
        }
    }

    #[test]
    fn go_resolves_package_import() {
        let known = set(&[
            "internal/service/server.go",
            "internal/service/client.go",
            "cmd/main.go",
        ]);
        let result = resolve_import("cmd/main.go", "internal/service", &known, &[]);
        // The representative is the first file in sorted order: deterministic.
        assert_eq!(result.as_deref(), Some("internal/service/client.go"));
    }

    #[test]
    fn go_multiple_packages_picks_shortest() {
        let known = set(&[
            "vendor/internal/service/server.go",
            "internal/service/client.go",
        ]);
        let result = resolve_import("cmd/main.go", "internal/service", &known, &[]);
        assert_eq!(result, Some("internal/service/client.go".to_string()));
    }

    #[test]
    fn go_resolves_package_with_module_prefix() {
        let known = set(&["github.com/org/repo/internal/service/server.go"]);
        let result = resolve_import(
            "github.com/org/repo/cmd/main.go",
            "internal/service",
            &known,
            &[],
        );
        assert_eq!(
            result,
            Some("github.com/org/repo/internal/service/server.go".to_string())
        );
    }

    #[test]
    fn go_unknown_package_returns_none() {
        let known = set(&["internal/other/thing.go"]);
        let result = resolve_import("cmd/main.go", "internal/service", &known, &[]);
        assert_eq!(result, None);
    }

    /// QA go1: `module example.com/go1` importing
    /// `example.com/go1/internal/calc` names the directory `internal/calc`,
    /// and the package is every non-test file in it.
    #[test]
    fn go_module_path_maps_to_every_file_of_the_package() {
        let known = [
            "cmd/app/main.go",
            "internal/calc/calc.go",
            "internal/calc/sum.go",
            "internal/calc/calc_test.go",
            "internal/calc/deep/other.go",
        ];
        let index = GoPackageIndex::new(known);
        let modules = [module("example.com/go1", "")];
        assert_eq!(
            index.package_files("example.com/go1/internal/calc", &modules),
            vec!["internal/calc/calc.go", "internal/calc/sum.go"]
        );
        assert_eq!(
            index.package_files("\"example.com/go1/internal/calc\"", &modules),
            vec!["internal/calc/calc.go", "internal/calc/sum.go"]
        );
        // The module root itself is a package too.
        let root = GoPackageIndex::new(["main.go", "util.go"]);
        assert_eq!(
            root.package_files("example.com/go1", &modules),
            vec!["main.go", "util.go"]
        );
    }

    /// With a known module, a path outside it is the standard library or an
    /// external module and must never land on a same-named local directory.
    #[test]
    fn go_module_rejects_foreign_paths_except_vendor() {
        let index = GoPackageIndex::new([
            "errors/errors.go",
            "internal/calc/calc.go",
            "vendor/github.com/x/y/y.go",
        ]);
        let modules = [module("example.com/go1", "")];
        assert!(index.package_files("errors", &modules).is_empty());
        assert!(
            index
                .package_files("github.com/other/internal/calc", &modules)
                .is_empty()
        );
        assert_eq!(
            index.package_files("github.com/x/y", &modules),
            vec!["vendor/github.com/x/y/y.go"]
        );
    }

    #[test]
    fn go_nested_module_wins_over_enclosing_module() {
        let index = GoPackageIndex::new(["tools/gen/gen.go", "tools/gen/sub/gen.go"]);
        let modules = [
            module("example.com/app/tools/gen", "tools/gen"),
            module("example.com/app", ""),
        ];
        assert_eq!(
            index.package_files("example.com/app/tools/gen/sub", &modules),
            vec!["tools/gen/sub/gen.go"]
        );
        let replaced = [module("example.com/lib", "third_party/lib")];
        let index = GoPackageIndex::new(["third_party/lib/lib.go"]);
        assert_eq!(
            index.package_files("example.com/lib", &replaced),
            vec!["third_party/lib/lib.go"]
        );
    }

    #[test]
    fn go_default_package_name_drops_major_versions() {
        assert_eq!(
            default_package_name("example.com/go1/internal/calc"),
            "calc"
        );
        assert_eq!(default_package_name("github.com/x/y/v2"), "y");
        assert_eq!(default_package_name("gopkg.in/yaml.v3"), "yaml");
        assert_eq!(default_package_name("fmt"), "fmt");
    }
}
