use std::collections::HashSet;

use crate::util::parent_dir;
use crate::workspace::{
    TsconfigAlias, WorkspaceContext, WorkspacePackage, extract_package_name,
    try_resolve_with_extensions,
};

/// Resolve a JavaScript/TypeScript import specifier to a file path.
///
/// For relative imports (starting with `.` or `..`):
///   - Try exact path
///   - Try with .js, .ts, .tsx, .jsx extensions
///   - Try as directory index: /index.js, /index.ts, /index.tsx, /index.jsx
///
/// For non-relative imports (bare specifiers):
///   - Try tsconfig path aliases (e.g., `@/utils` -> `src/utils`)
///   - Try workspace package resolution (e.g., `@myorg/shared` -> `packages/shared/src/index.ts`)
///   - Otherwise return None (external package)
pub fn resolve_import(
    from_file: &str,
    specifier: &str,
    known_files: &HashSet<&str>,
    workspace_ctx: &WorkspaceContext,
) -> Option<String> {
    if specifier.starts_with('.') {
        return resolve_relative(from_file, specifier, known_files);
    }

    // Non-relative: try tsconfig path aliases first
    if let Some(resolved) = resolve_tsconfig_alias(specifier, known_files, &workspace_ctx.aliases) {
        return Some(resolved);
    }

    // Try workspace package resolution
    if let Some(resolved) =
        resolve_workspace_package(specifier, known_files, &workspace_ctx.packages)
    {
        return Some(resolved);
    }

    // External package — cannot resolve
    None
}

/// Resolve a relative import specifier.
fn resolve_relative(
    from_file: &str,
    specifier: &str,
    known_files: &HashSet<&str>,
) -> Option<String> {
    let base_dir = parent_dir(from_file);
    let joined = join_path(base_dir, specifier);
    let normalized = normalize_path(&joined);

    // Try exact path
    if known_files.contains(&normalized.as_str()) {
        return Some(normalized);
    }

    // Try with extensions
    for ext in &[".js", ".ts", ".tsx", ".jsx"] {
        let candidate = format!("{normalized}{ext}");
        if known_files.contains(&candidate.as_str()) {
            return Some(candidate);
        }
    }

    // Try as directory index
    for ext in &["/index.js", "/index.ts", "/index.tsx", "/index.jsx"] {
        let candidate = format!("{normalized}{ext}");
        if known_files.contains(&candidate.as_str()) {
            return Some(candidate);
        }
    }

    None
}

/// Resolve a specifier against tsconfig path aliases.
fn resolve_tsconfig_alias(
    specifier: &str,
    known_files: &HashSet<&str>,
    aliases: &[TsconfigAlias],
) -> Option<String> {
    for alias in aliases {
        if let Some(rest) = specifier.strip_prefix(&alias.prefix) {
            let rewritten = format!("{}{}", alias.target, rest);
            if let Some(resolved) = try_resolve_with_extensions(&rewritten, known_files) {
                return Some(resolved);
            }
        }
    }
    None
}

/// Resolve a bare specifier against workspace packages.
fn resolve_workspace_package(
    specifier: &str,
    known_files: &HashSet<&str>,
    packages: &[WorkspacePackage],
) -> Option<String> {
    let pkg_name = extract_package_name(specifier);
    let pkg = packages.iter().find(|p| p.name == pkg_name)?;

    // If the specifier has a subpath beyond the package name, try to resolve it
    let subpath = if specifier.len() > pkg_name.len() {
        // Strip the leading / from the subpath
        &specifier[pkg_name.len() + 1..]
    } else {
        ""
    };

    if !subpath.is_empty() {
        // Try resolving the subpath within the package directory
        let candidate = format!("{}/{}", pkg.directory, subpath);
        if let Some(resolved) = try_resolve_with_extensions(&candidate, known_files) {
            return Some(resolved);
        }
        // Also try under src/
        let candidate = format!("{}/src/{}", pkg.directory, subpath);
        if let Some(resolved) = try_resolve_with_extensions(&candidate, known_files) {
            return Some(resolved);
        }
    }

    // Try common entry points for the package root
    let entry_suffixes = [
        "src/index.ts",
        "src/index.tsx",
        "src/index.js",
        "src/index.jsx",
        "index.ts",
        "index.tsx",
        "index.js",
        "index.jsx",
        "src/main.ts",
        "src/main.js",
        "lib/index.ts",
        "lib/index.js",
    ];

    for suffix in &entry_suffixes {
        let candidate = format!("{}/{suffix}", pkg.directory);
        if known_files.contains(&candidate.as_str()) {
            return Some(candidate);
        }
    }

    None
}

fn join_path(base: &str, rel: &str) -> String {
    if base.is_empty() {
        rel.to_string()
    } else {
        format!("{base}/{rel}")
    }
}

/// Normalize path components, resolving `.` and `..` segments.
fn normalize_path(path: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();
    for segment in path.split('/') {
        match segment {
            "." | "" => {}
            ".." => {
                parts.pop();
            }
            s => parts.push(s),
        }
    }
    parts.join("/")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn empty_ctx() -> WorkspaceContext {
        WorkspaceContext::default()
    }

    fn set<'a>(files: &[&'a str]) -> HashSet<&'a str> {
        files.iter().copied().collect()
    }

    // ── Relative imports ──────────────────────────────────────────────────

    #[test]
    fn resolves_relative_import_js() {
        let known = set(&["src/helper.js", "src/index.js"]);
        let result = resolve_import("src/main.js", "./helper", &known, &empty_ctx());
        assert_eq!(result, Some("src/helper.js".to_string()));
    }

    #[test]
    fn resolves_relative_import_ts() {
        let known = set(&["src/utils.ts"]);
        let result = resolve_import("src/main.ts", "./utils", &known, &empty_ctx());
        assert_eq!(result, Some("src/utils.ts".to_string()));
    }

    #[test]
    fn resolves_barrel_import_js() {
        let known = set(&["src/utils/index.js", "src/utils/helper.js"]);
        let result = resolve_import("src/main.js", "./utils", &known, &empty_ctx());
        assert_eq!(result, Some("src/utils/index.js".to_string()));
    }

    #[test]
    fn resolves_parent_dir_import() {
        let known = set(&["src/shared.ts", "src/components/Button.tsx"]);
        let result = resolve_import(
            "src/components/Button.tsx",
            "../shared",
            &known,
            &empty_ctx(),
        );
        assert_eq!(result, Some("src/shared.ts".to_string()));
    }

    #[test]
    fn non_relative_import_returns_none_without_context() {
        let known = set(&["node_modules/lodash/index.js"]);
        let result = resolve_import("src/main.js", "lodash", &known, &empty_ctx());
        assert_eq!(result, None);
    }

    #[test]
    fn unknown_file_returns_none() {
        let known: HashSet<&str> = HashSet::new();
        let result = resolve_import("src/main.js", "./missing", &known, &empty_ctx());
        assert_eq!(result, None);
    }

    // ── tsconfig alias resolution ─────────────────────────────────────────

    #[test]
    fn resolves_tsconfig_alias() {
        let known = set(&["src/utils/helpers.ts"]);
        let ctx = WorkspaceContext {
            packages: vec![],
            aliases: vec![TsconfigAlias {
                prefix: "@/".to_string(),
                target: "src/".to_string(),
            }],
        };
        let result = resolve_import("src/main.ts", "@/utils/helpers", &known, &ctx);
        assert_eq!(result, Some("src/utils/helpers.ts".to_string()));
    }

    #[test]
    fn resolves_tsconfig_alias_to_directory_index() {
        let known = set(&["src/utils/index.ts"]);
        let ctx = WorkspaceContext {
            packages: vec![],
            aliases: vec![TsconfigAlias {
                prefix: "@/".to_string(),
                target: "src/".to_string(),
            }],
        };
        let result = resolve_import("src/main.ts", "@/utils", &known, &ctx);
        assert_eq!(result, Some("src/utils/index.ts".to_string()));
    }

    #[test]
    fn resolves_tilde_alias() {
        let known = set(&["lib/core.ts"]);
        let ctx = WorkspaceContext {
            packages: vec![],
            aliases: vec![TsconfigAlias {
                prefix: "~/".to_string(),
                target: "lib/".to_string(),
            }],
        };
        let result = resolve_import("src/main.ts", "~/core", &known, &ctx);
        assert_eq!(result, Some("lib/core.ts".to_string()));
    }

    #[test]
    fn alias_not_matching_returns_none() {
        let known = set(&["src/utils.ts"]);
        let ctx = WorkspaceContext {
            packages: vec![],
            aliases: vec![TsconfigAlias {
                prefix: "@/".to_string(),
                target: "src/".to_string(),
            }],
        };
        let result = resolve_import("src/main.ts", "lodash", &known, &ctx);
        assert_eq!(result, None);
    }

    // ── Workspace package resolution ──────────────────────────────────────

    #[test]
    fn resolves_workspace_package_to_entry() {
        let known = set(&[
            "packages/shared/src/index.ts",
            "packages/shared/src/utils.ts",
            "apps/web/src/main.ts",
        ]);
        let ctx = WorkspaceContext {
            packages: vec![WorkspacePackage {
                name: "@myorg/shared".to_string(),
                directory: "packages/shared".to_string(),
            }],
            aliases: vec![],
        };
        let result = resolve_import("apps/web/src/main.ts", "@myorg/shared", &known, &ctx);
        assert_eq!(result, Some("packages/shared/src/index.ts".to_string()));
    }

    #[test]
    fn resolves_workspace_package_with_subpath() {
        let known = set(&[
            "packages/shared/src/index.ts",
            "packages/shared/src/utils.ts",
        ]);
        let ctx = WorkspaceContext {
            packages: vec![WorkspacePackage {
                name: "@myorg/shared".to_string(),
                directory: "packages/shared".to_string(),
            }],
            aliases: vec![],
        };
        let result = resolve_import(
            "apps/web/src/main.ts",
            "@myorg/shared/src/utils",
            &known,
            &ctx,
        );
        assert_eq!(result, Some("packages/shared/src/utils.ts".to_string()));
    }

    #[test]
    fn resolves_unscoped_workspace_package() {
        let known = set(&["packages/utils/index.ts"]);
        let ctx = WorkspaceContext {
            packages: vec![WorkspacePackage {
                name: "my-utils".to_string(),
                directory: "packages/utils".to_string(),
            }],
            aliases: vec![],
        };
        let result = resolve_import("apps/web/src/main.ts", "my-utils", &known, &ctx);
        assert_eq!(result, Some("packages/utils/index.ts".to_string()));
    }

    #[test]
    fn workspace_package_not_found_returns_none() {
        let known = set(&["packages/shared/src/index.ts"]);
        let ctx = WorkspaceContext {
            packages: vec![WorkspacePackage {
                name: "@myorg/shared".to_string(),
                directory: "packages/shared".to_string(),
            }],
            aliases: vec![],
        };
        let result = resolve_import("apps/web/src/main.ts", "@myorg/other", &known, &ctx);
        assert_eq!(result, None);
    }

    // ── Combined: alias + workspace ───────────────────────────────────────

    #[test]
    fn alias_takes_priority_over_workspace() {
        // When both alias and workspace could match, alias wins
        let known = set(&["src/shared/index.ts", "packages/shared/src/index.ts"]);
        let ctx = WorkspaceContext {
            packages: vec![WorkspacePackage {
                name: "@/shared".to_string(),
                directory: "packages/shared".to_string(),
            }],
            aliases: vec![TsconfigAlias {
                prefix: "@/".to_string(),
                target: "src/".to_string(),
            }],
        };
        let result = resolve_import("apps/web/main.ts", "@/shared", &known, &ctx);
        // Alias resolves "@/shared" -> "src/shared" -> "src/shared/index.ts"
        assert_eq!(result, Some("src/shared/index.ts".to_string()));
    }
}
