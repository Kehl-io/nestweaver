//! Monorepo workspace package detection and tsconfig path alias resolution.
//!
//! Discovers workspace packages from:
//! - npm/yarn: `"workspaces"` field in root `package.json`
//! - pnpm: `packages` field in `pnpm-workspace.yaml`
//!
//! Parses tsconfig path aliases from:
//! - `tsconfig.json`, `tsconfig.app.json`, `tsconfig.base.json`

use std::collections::{HashMap, HashSet};
use std::path::Path;

/// A workspace package discovered from package.json workspaces or pnpm-workspace.yaml.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspacePackage {
    /// The npm package name (e.g., `@myorg/shared`).
    pub name: String,
    /// Repo-relative directory (e.g., `packages/shared`).
    pub directory: String,
}

/// A tsconfig path alias mapping (e.g., `@/*` -> `src/*`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TsconfigAlias {
    /// The import prefix to match (e.g., `@/`).
    pub prefix: String,
    /// The target path prefix to substitute (e.g., `src/`).
    pub target: String,
}

/// Context for resolving monorepo imports, loaded once per indexing run.
#[derive(Debug, Clone, Default)]
pub struct WorkspaceContext {
    pub packages: Vec<WorkspacePackage>,
    pub aliases: Vec<TsconfigAlias>,
}

impl WorkspaceContext {
    /// Returns true if there are no workspace packages and no tsconfig aliases.
    pub fn is_empty(&self) -> bool {
        self.packages.is_empty() && self.aliases.is_empty()
    }
}

/// Discover workspace packages and tsconfig aliases from a repo root.
///
/// This is the main entry point. It reads the filesystem to discover
/// monorepo layout and path alias configuration.
pub fn discover_workspace_context(repo_path: &Path) -> WorkspaceContext {
    let packages = discover_workspace_packages(repo_path);
    let aliases = load_tsconfig_aliases(repo_path);
    WorkspaceContext { packages, aliases }
}

/// Discover workspace context using a content reader function.
///
/// Unlike [`discover_workspace_context`], this works with bare git repos
/// by reading files through the provided `read_file` callback instead of
/// direct filesystem access. The callback takes a repo-relative path and
/// returns `Ok(contents)` or an error.
///
/// Note: workspace *glob expansion* (e.g. `packages/*`) requires directory
/// listing which is not available through the reader; only the root-level
/// package.json workspaces/tsconfig aliases are discovered. This is
/// sufficient for the common case where the root package.json explicitly
/// names workspace directories.
pub fn discover_workspace_context_with<F>(read_file: F) -> WorkspaceContext
where
    F: Fn(&Path) -> Result<String, std::io::Error>,
{
    let packages = discover_workspace_packages_with(&read_file);
    let aliases = load_tsconfig_aliases_with(&read_file);
    WorkspaceContext { packages, aliases }
}

/// Discover workspace packages using a content reader function.
///
/// Reads `package.json` via the callback. Cannot expand glob patterns
/// (that requires directory listing), so only explicit workspace paths
/// are resolved.
fn discover_workspace_packages_with<F>(read_file: &F) -> Vec<WorkspacePackage>
where
    F: Fn(&Path) -> Result<String, std::io::Error>,
{
    let mut packages = Vec::new();

    // Try npm/yarn workspaces from root package.json
    if let Ok(contents) = read_file(Path::new("package.json"))
        && let Ok(root) = serde_json::from_str::<serde_json::Value>(&contents)
        && let Some(globs) = extract_workspace_globs(&root)
    {
        // For bare repos we can't list directories, but we can try
        // reading package.json from explicitly named (non-glob) paths.
        for glob in &globs {
            let clean = glob.trim_end_matches('/');
            if clean.contains('*') {
                // Skip glob patterns — can't enumerate without directory listing.
                continue;
            }
            let pkg_json_rel = format!("{clean}/package.json");
            if let Ok(pkg_contents) = read_file(Path::new(&pkg_json_rel))
                && let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&pkg_contents)
                && let Some(name) = parsed.get("name").and_then(|v| v.as_str())
            {
                packages.push(WorkspacePackage {
                    name: name.to_string(),
                    directory: clean.to_string(),
                });
            }
        }
    }

    // Try pnpm-workspace.yaml
    if packages.is_empty()
        && let Ok(contents) = read_file(Path::new("pnpm-workspace.yaml"))
        && let Ok(yaml) = serde_yaml::from_str::<serde_json::Value>(&contents)
        && let Some(pkgs) = yaml.get("packages").and_then(|v| v.as_array())
    {
        for v in pkgs {
            if let Some(path) = v.as_str() {
                let clean = path.trim_end_matches('/');
                if clean.contains('*') {
                    continue;
                }
                let pkg_json_rel = format!("{clean}/package.json");
                if let Ok(pkg_contents) = read_file(Path::new(&pkg_json_rel))
                    && let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&pkg_contents)
                    && let Some(name) = parsed.get("name").and_then(|v| v.as_str())
                {
                    packages.push(WorkspacePackage {
                        name: name.to_string(),
                        directory: clean.to_string(),
                    });
                }
            }
        }
    }

    packages
}

/// Load tsconfig aliases using a content reader function.
fn load_tsconfig_aliases_with<F>(read_file: &F) -> Vec<TsconfigAlias>
where
    F: Fn(&Path) -> Result<String, std::io::Error>,
{
    let candidates = ["tsconfig.json", "tsconfig.app.json", "tsconfig.base.json"];

    for candidate in &candidates {
        if let Ok(contents) = read_file(Path::new(candidate)) {
            let stripped = strip_json_comments(&contents);
            if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&stripped) {
                let mut aliases = extract_tsconfig_aliases(&parsed, "");

                // Follow `extends` chain (one level deep for safety)
                if aliases.is_empty()
                    && let Some(extends) = parsed.get("extends").and_then(|v| v.as_str())
                {
                    // Resolve relative to the tsconfig's directory (which is
                    // the repo root for top-level configs).
                    let base_rel = Path::new(candidate)
                        .parent()
                        .unwrap_or(Path::new(""))
                        .join(extends);
                    if let Ok(base_contents) = read_file(&base_rel) {
                        let base_stripped = strip_json_comments(&base_contents);
                        if let Ok(base_parsed) =
                            serde_json::from_str::<serde_json::Value>(&base_stripped)
                        {
                            let base_url = parsed
                                .get("compilerOptions")
                                .and_then(|co| co.get("baseUrl"))
                                .and_then(|v| v.as_str())
                                .unwrap_or("");
                            aliases = extract_tsconfig_aliases(&base_parsed, base_url);
                        }
                    }
                }

                if !aliases.is_empty() {
                    return aliases;
                }
            }
        }
    }

    Vec::new()
}

/// Discover workspace packages from the repo root.
///
/// Checks npm/yarn workspaces in `package.json` and pnpm `pnpm-workspace.yaml`.
pub fn discover_workspace_packages(repo_path: &Path) -> Vec<WorkspacePackage> {
    let mut packages = Vec::new();

    // Try npm/yarn workspaces from root package.json
    let pkg_json_path = repo_path.join("package.json");
    if let Ok(contents) = std::fs::read_to_string(&pkg_json_path)
        && let Ok(root) = serde_json::from_str::<serde_json::Value>(&contents)
        && let Some(globs) = extract_workspace_globs(&root)
    {
        expand_workspace_globs(repo_path, &globs, &mut packages);
    }

    // Try pnpm-workspace.yaml
    if packages.is_empty() {
        let pnpm_path = repo_path.join("pnpm-workspace.yaml");
        if let Ok(contents) = std::fs::read_to_string(&pnpm_path)
            && let Ok(yaml) = serde_yaml::from_str::<serde_json::Value>(&contents)
            && let Some(pkgs) = yaml.get("packages").and_then(|v| v.as_array())
        {
            let globs: Vec<String> = pkgs
                .iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect();
            expand_workspace_globs(repo_path, &globs, &mut packages);
        }
    }

    packages
}

/// Extract workspace glob patterns from a root package.json value.
fn extract_workspace_globs(root: &serde_json::Value) -> Option<Vec<String>> {
    let workspaces = root.get("workspaces")?;

    // npm/yarn format: "workspaces": ["packages/*"]
    if let Some(arr) = workspaces.as_array() {
        let globs: Vec<String> = arr
            .iter()
            .filter_map(|v| v.as_str().map(|s| s.to_string()))
            .collect();
        if !globs.is_empty() {
            return Some(globs);
        }
    }

    // yarn v2+ format: "workspaces": { "packages": ["packages/*"] }
    if let Some(obj) = workspaces.as_object()
        && let Some(pkgs) = obj.get("packages").and_then(|v| v.as_array())
    {
        let globs: Vec<String> = pkgs
            .iter()
            .filter_map(|v| v.as_str().map(|s| s.to_string()))
            .collect();
        if !globs.is_empty() {
            return Some(globs);
        }
    }

    None
}

/// Expand workspace glob patterns (e.g., `packages/*`) to concrete packages.
///
/// For each glob pattern, finds directories containing a `package.json` and
/// reads the package name from it.
fn expand_workspace_globs(
    repo_path: &Path,
    globs: &[String],
    packages: &mut Vec<WorkspacePackage>,
) {
    for glob in globs {
        // Handle simple glob patterns like "packages/*" or "apps/*"
        // by finding the parent directory and listing its children.
        let clean = glob.trim_end_matches('/');

        if let Some(parent) = clean.strip_suffix("/*") {
            // e.g., "packages/*" -> list all dirs under "packages/"
            let search_dir = repo_path.join(parent);
            if let Ok(entries) = std::fs::read_dir(&search_dir) {
                for entry in entries.flatten() {
                    if entry.file_type().is_ok_and(|ft| ft.is_dir()) {
                        let dir_path = entry.path();
                        let pkg_json = dir_path.join("package.json");
                        if let Some(pkg) = read_package_name(&pkg_json) {
                            let rel_dir = dir_path
                                .strip_prefix(repo_path)
                                .unwrap_or(&dir_path)
                                .to_string_lossy()
                                .replace('\\', "/");
                            packages.push(WorkspacePackage {
                                name: pkg,
                                directory: rel_dir,
                            });
                        }
                    }
                }
            }
        } else if let Some(parent) = clean.strip_suffix("/**") {
            // e.g., "packages/**" -> recurse into subdirectories
            let search_dir = repo_path.join(parent);
            collect_packages_recursive(repo_path, &search_dir, packages, 0);
        } else {
            // Exact path, e.g., "tools/cli"
            let dir_path = repo_path.join(clean);
            let pkg_json = dir_path.join("package.json");
            if let Some(pkg) = read_package_name(&pkg_json) {
                let rel_dir = dir_path
                    .strip_prefix(repo_path)
                    .unwrap_or(&dir_path)
                    .to_string_lossy()
                    .replace('\\', "/");
                packages.push(WorkspacePackage {
                    name: pkg,
                    directory: rel_dir,
                });
            }
        }
    }
}

/// Maximum recursion depth for workspace package discovery. Prevents
/// runaway traversal in deeply nested or symlinked directory trees.
const MAX_PACKAGE_RECURSION_DEPTH: usize = 10;

/// Directories that are never useful for workspace package discovery.
const SKIP_DIRS: &[&str] = &["node_modules", ".git", "target", "build", "dist"];

/// Recursively collect packages from a directory tree.
fn collect_packages_recursive(
    repo_path: &Path,
    dir: &Path,
    packages: &mut Vec<WorkspacePackage>,
    depth: usize,
) {
    if depth >= MAX_PACKAGE_RECURSION_DEPTH {
        return;
    }
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            if entry.file_type().is_ok_and(|ft| ft.is_dir()) {
                let dir_path = entry.path();
                // Skip well-known non-source directories
                if dir_path
                    .file_name()
                    .is_some_and(|n| SKIP_DIRS.iter().any(|&s| n == s))
                {
                    continue;
                }
                let pkg_json = dir_path.join("package.json");
                if let Some(pkg) = read_package_name(&pkg_json) {
                    let rel_dir = dir_path
                        .strip_prefix(repo_path)
                        .unwrap_or(&dir_path)
                        .to_string_lossy()
                        .replace('\\', "/");
                    packages.push(WorkspacePackage {
                        name: pkg,
                        directory: rel_dir,
                    });
                }
                // Continue recursing even if this dir has a package.json
                // (nested workspaces)
                collect_packages_recursive(repo_path, &dir_path, packages, depth + 1);
            }
        }
    }
}

/// Read the "name" field from a package.json file.
fn read_package_name(pkg_json_path: &Path) -> Option<String> {
    let contents = std::fs::read_to_string(pkg_json_path).ok()?;
    let parsed: serde_json::Value = serde_json::from_str(&contents).ok()?;
    parsed
        .get("name")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
}

/// Load tsconfig path aliases from the repo root.
///
/// Tries the following tsconfig files in order:
/// - `tsconfig.json`
/// - `tsconfig.app.json`
/// - `tsconfig.base.json`
///
/// Handles JSON with comments (strips `//` line comments and `/* */` block comments).
pub fn load_tsconfig_aliases(repo_path: &Path) -> Vec<TsconfigAlias> {
    let candidates = ["tsconfig.json", "tsconfig.app.json", "tsconfig.base.json"];

    for candidate in &candidates {
        let tsconfig_path = repo_path.join(candidate);
        if let Ok(contents) = std::fs::read_to_string(&tsconfig_path) {
            let stripped = strip_json_comments(&contents);
            if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&stripped) {
                let mut aliases = extract_tsconfig_aliases(&parsed, "");

                // Follow `extends` chain (one level deep for safety)
                if aliases.is_empty()
                    && let Some(extends) = parsed.get("extends").and_then(|v| v.as_str())
                {
                    let base_path = tsconfig_path.parent().unwrap_or(repo_path).join(extends);
                    if let Ok(base_contents) = std::fs::read_to_string(&base_path) {
                        let base_stripped = strip_json_comments(&base_contents);
                        if let Ok(base_parsed) =
                            serde_json::from_str::<serde_json::Value>(&base_stripped)
                        {
                            let base_url = parsed
                                .get("compilerOptions")
                                .and_then(|co| co.get("baseUrl"))
                                .and_then(|v| v.as_str())
                                .unwrap_or("");
                            aliases = extract_tsconfig_aliases(&base_parsed, base_url);
                        }
                    }
                }

                if !aliases.is_empty() {
                    return aliases;
                }
            }
        }
    }

    Vec::new()
}

/// Extract path aliases from a parsed tsconfig JSON value.
fn extract_tsconfig_aliases(
    parsed: &serde_json::Value,
    override_base_url: &str,
) -> Vec<TsconfigAlias> {
    let compiler_options = match parsed.get("compilerOptions") {
        Some(co) => co,
        None => return Vec::new(),
    };

    let base_url = if override_base_url.is_empty() {
        compiler_options
            .get("baseUrl")
            .and_then(|v| v.as_str())
            .unwrap_or(".")
    } else {
        override_base_url
    };

    let paths = match compiler_options.get("paths").and_then(|v| v.as_object()) {
        Some(p) => p,
        None => return Vec::new(),
    };

    let mut aliases = Vec::new();

    for (pattern, targets) in paths {
        let targets = match targets.as_array() {
            Some(arr) => arr,
            None => continue,
        };

        // Only handle wildcard patterns: "@/*" -> ["src/*"]
        if let Some(prefix) = pattern.strip_suffix('*') {
            for target in targets {
                if let Some(target_str) = target.as_str()
                    && let Some(target_prefix) = target_str.strip_suffix('*')
                {
                    // Resolve the target relative to baseUrl
                    let resolved_target = if base_url == "." || base_url.is_empty() {
                        target_prefix.to_string()
                    } else {
                        let base = base_url.trim_end_matches('/');
                        let target_trimmed = target_prefix.trim_start_matches("./");
                        if target_trimmed.is_empty() {
                            format!("{base}/")
                        } else {
                            format!("{base}/{target_trimmed}")
                        }
                    };

                    aliases.push(TsconfigAlias {
                        prefix: prefix.to_string(),
                        target: resolved_target,
                    });
                    // Only use the first target for each pattern
                    break;
                }
            }
        }
    }

    aliases
}

/// Strip JSON comments from a string.
///
/// Handles both `//` line comments and `/* */` block comments.
/// Preserves string contents (doesn't strip inside quoted strings).
fn strip_json_comments(input: &str) -> String {
    let mut output = String::with_capacity(input.len());
    let chars: Vec<char> = input.chars().collect();
    let len = chars.len();
    let mut i = 0;

    while i < len {
        // Check for strings — preserve them as-is
        if chars[i] == '"' {
            output.push(chars[i]);
            i += 1;
            while i < len && chars[i] != '"' {
                if chars[i] == '\\' && i + 1 < len {
                    output.push(chars[i]);
                    output.push(chars[i + 1]);
                    i += 2;
                } else {
                    output.push(chars[i]);
                    i += 1;
                }
            }
            if i < len {
                output.push(chars[i]); // closing quote
                i += 1;
            }
            continue;
        }

        // Check for line comments
        if chars[i] == '/' && i + 1 < len && chars[i + 1] == '/' {
            // Skip until end of line
            i += 2;
            while i < len && chars[i] != '\n' {
                i += 1;
            }
            continue;
        }

        // Check for block comments
        if chars[i] == '/' && i + 1 < len && chars[i + 1] == '*' {
            i += 2;
            while i + 1 < len && !(chars[i] == '*' && chars[i + 1] == '/') {
                i += 1;
            }
            if i + 1 < len {
                i += 2; // skip */
            }
            continue;
        }

        output.push(chars[i]);
        i += 1;
    }

    output
}

/// Extract the package name from a bare import specifier.
///
/// - `@scope/name/path` -> `@scope/name`
/// - `@scope/name` -> `@scope/name`
/// - `lodash/fp` -> `lodash`
/// - `lodash` -> `lodash`
pub fn extract_package_name(specifier: &str) -> &str {
    if let Some(rest) = specifier.strip_prefix('@') {
        // Scoped package: @scope/name[/path]
        if let Some(slash_pos) = rest.find('/') {
            let after_scope = &rest[slash_pos + 1..];
            if let Some(second_slash) = after_scope.find('/') {
                // @scope/name/subpath -> @scope/name
                &specifier[..1 + slash_pos + 1 + second_slash]
            } else {
                // @scope/name (no subpath)
                specifier
            }
        } else {
            // Just @scope with no name — unusual but return as-is
            specifier
        }
    } else {
        // Unscoped package: name[/path]
        match specifier.find('/') {
            Some(pos) => &specifier[..pos],
            None => specifier,
        }
    }
}

/// Try to resolve a path against known files, attempting various extensions.
///
/// This is similar to Node.js module resolution: tries exact, then with
/// `.ts`, `.tsx`, `.js`, `.jsx` extensions, then as a directory index.
pub fn try_resolve_with_extensions(path: &str, known_files: &HashSet<&str>) -> Option<String> {
    // Try exact
    if known_files.contains(path) {
        return Some(path.to_string());
    }

    // Try with extensions
    for ext in &[".ts", ".tsx", ".js", ".jsx"] {
        let candidate = format!("{path}{ext}");
        if known_files.contains(&candidate.as_str()) {
            return Some(candidate);
        }
    }

    // Try as directory index
    for suffix in &["/index.ts", "/index.tsx", "/index.js", "/index.jsx"] {
        let candidate = format!("{path}{suffix}");
        if known_files.contains(&candidate.as_str()) {
            return Some(candidate);
        }
    }

    None
}

/// Build a lookup from package name to workspace package for efficient resolution.
pub fn build_package_lookup(packages: &[WorkspacePackage]) -> HashMap<&str, &WorkspacePackage> {
    packages.iter().map(|p| (p.name.as_str(), p)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── extract_package_name ──────────────────────────────────────────────

    #[test]
    fn extract_scoped_package() {
        assert_eq!(extract_package_name("@myorg/shared"), "@myorg/shared");
    }

    #[test]
    fn extract_scoped_package_with_subpath() {
        assert_eq!(extract_package_name("@myorg/shared/utils"), "@myorg/shared");
    }

    #[test]
    fn extract_unscoped_package() {
        assert_eq!(extract_package_name("lodash"), "lodash");
    }

    #[test]
    fn extract_unscoped_package_with_subpath() {
        assert_eq!(extract_package_name("lodash/fp"), "lodash");
    }

    // ── strip_json_comments ───────────────────────────────────────────────

    #[test]
    fn strip_line_comments() {
        let input = r#"{
  // This is a comment
  "key": "value"
}"#;
        let stripped = strip_json_comments(input);
        assert!(!stripped.contains("// This"));
        assert!(stripped.contains("\"key\""));
    }

    #[test]
    fn strip_block_comments() {
        let input = r#"{
  /* block comment */
  "key": "value"
}"#;
        let stripped = strip_json_comments(input);
        assert!(!stripped.contains("block comment"));
        assert!(stripped.contains("\"key\""));
    }

    #[test]
    fn preserves_strings_with_slashes() {
        let input = r#"{"url": "https://example.com"}"#;
        let stripped = strip_json_comments(input);
        assert_eq!(stripped, input);
    }

    // ── extract_tsconfig_aliases ──────────────────────────────────────────

    #[test]
    fn extracts_path_alias() {
        let tsconfig: serde_json::Value = serde_json::from_str(
            r#"{
                "compilerOptions": {
                    "baseUrl": ".",
                    "paths": {
                        "@/*": ["src/*"]
                    }
                }
            }"#,
        )
        .unwrap();

        let aliases = extract_tsconfig_aliases(&tsconfig, "");
        assert_eq!(aliases.len(), 1);
        assert_eq!(aliases[0].prefix, "@/");
        assert_eq!(aliases[0].target, "src/");
    }

    #[test]
    fn extracts_multiple_aliases() {
        let tsconfig: serde_json::Value = serde_json::from_str(
            r#"{
                "compilerOptions": {
                    "baseUrl": ".",
                    "paths": {
                        "@/*": ["src/*"],
                        "~/*": ["lib/*"]
                    }
                }
            }"#,
        )
        .unwrap();

        let aliases = extract_tsconfig_aliases(&tsconfig, "");
        assert_eq!(aliases.len(), 2);
    }

    #[test]
    fn handles_base_url_in_alias() {
        let tsconfig: serde_json::Value = serde_json::from_str(
            r#"{
                "compilerOptions": {
                    "baseUrl": "src",
                    "paths": {
                        "@/*": ["./*"]
                    }
                }
            }"#,
        )
        .unwrap();

        let aliases = extract_tsconfig_aliases(&tsconfig, "");
        assert_eq!(aliases.len(), 1);
        assert_eq!(aliases[0].prefix, "@/");
        assert_eq!(aliases[0].target, "src/");
    }

    // ── extract_workspace_globs ───────────────────────────────────────────

    #[test]
    fn extracts_npm_workspace_globs() {
        let pkg: serde_json::Value =
            serde_json::from_str(r#"{"workspaces": ["packages/*", "apps/*"]}"#).unwrap();
        let globs = extract_workspace_globs(&pkg).unwrap();
        assert_eq!(globs, vec!["packages/*", "apps/*"]);
    }

    #[test]
    fn extracts_yarn_v2_workspace_globs() {
        let pkg: serde_json::Value =
            serde_json::from_str(r#"{"workspaces": {"packages": ["packages/*"]}}"#).unwrap();
        let globs = extract_workspace_globs(&pkg).unwrap();
        assert_eq!(globs, vec!["packages/*"]);
    }

    #[test]
    fn no_workspace_field_returns_none() {
        let pkg: serde_json::Value = serde_json::from_str(r#"{"name": "foo"}"#).unwrap();
        assert!(extract_workspace_globs(&pkg).is_none());
    }

    // ── try_resolve_with_extensions ───────────────────────────────────────

    fn set<'a>(files: &[&'a str]) -> HashSet<&'a str> {
        files.iter().copied().collect()
    }

    #[test]
    fn resolves_exact() {
        let known = set(&["src/utils.ts"]);
        assert_eq!(
            try_resolve_with_extensions("src/utils.ts", &known),
            Some("src/utils.ts".to_string())
        );
    }

    #[test]
    fn resolves_with_ts_extension() {
        let known = set(&["src/utils.ts"]);
        assert_eq!(
            try_resolve_with_extensions("src/utils", &known),
            Some("src/utils.ts".to_string())
        );
    }

    #[test]
    fn resolves_directory_index() {
        let known = set(&["src/utils/index.ts"]);
        assert_eq!(
            try_resolve_with_extensions("src/utils", &known),
            Some("src/utils/index.ts".to_string())
        );
    }

    // ── filesystem-based tests ────────────────────────────────────────────

    #[test]
    fn discover_packages_from_npm_workspaces() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        // Write root package.json with workspaces
        std::fs::write(
            root.join("package.json"),
            r#"{"workspaces": ["packages/*"]}"#,
        )
        .unwrap();

        // Create packages/shared with package.json
        let pkg_dir = root.join("packages").join("shared");
        std::fs::create_dir_all(&pkg_dir).unwrap();
        std::fs::write(pkg_dir.join("package.json"), r#"{"name": "@myorg/shared"}"#).unwrap();

        // Create packages/ui with package.json
        let ui_dir = root.join("packages").join("ui");
        std::fs::create_dir_all(&ui_dir).unwrap();
        std::fs::write(ui_dir.join("package.json"), r#"{"name": "@myorg/ui"}"#).unwrap();

        let packages = discover_workspace_packages(root);
        assert_eq!(packages.len(), 2);

        let names: Vec<&str> = packages.iter().map(|p| p.name.as_str()).collect();
        assert!(names.contains(&"@myorg/shared"));
        assert!(names.contains(&"@myorg/ui"));
    }

    #[test]
    fn discover_packages_from_pnpm() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        // Write pnpm-workspace.yaml
        std::fs::write(
            root.join("pnpm-workspace.yaml"),
            "packages:\n  - 'packages/*'\n",
        )
        .unwrap();

        // Create packages/lib with package.json
        let lib_dir = root.join("packages").join("lib");
        std::fs::create_dir_all(&lib_dir).unwrap();
        std::fs::write(lib_dir.join("package.json"), r#"{"name": "my-lib"}"#).unwrap();

        let packages = discover_workspace_packages(root);
        assert_eq!(packages.len(), 1);
        assert_eq!(packages[0].name, "my-lib");
    }

    #[test]
    fn load_tsconfig_from_file() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        std::fs::write(
            root.join("tsconfig.json"),
            r#"{
                // This is a comment
                "compilerOptions": {
                    "baseUrl": ".",
                    "paths": {
                        "@/*": ["src/*"]
                    }
                }
            }"#,
        )
        .unwrap();

        let aliases = load_tsconfig_aliases(root);
        assert_eq!(aliases.len(), 1);
        assert_eq!(aliases[0].prefix, "@/");
        assert_eq!(aliases[0].target, "src/");
    }

    #[test]
    fn load_tsconfig_follows_extends() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        // tsconfig.json extends tsconfig.base.json but has no paths itself
        std::fs::write(
            root.join("tsconfig.json"),
            r#"{"extends": "./tsconfig.base.json", "compilerOptions": {}}"#,
        )
        .unwrap();

        std::fs::write(
            root.join("tsconfig.base.json"),
            r#"{
                "compilerOptions": {
                    "baseUrl": ".",
                    "paths": {
                        "~/*": ["lib/*"]
                    }
                }
            }"#,
        )
        .unwrap();

        let aliases = load_tsconfig_aliases(root);
        assert_eq!(aliases.len(), 1);
        assert_eq!(aliases[0].prefix, "~/");
        assert_eq!(aliases[0].target, "lib/");
    }

    #[test]
    fn discover_full_workspace_context() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        // package.json with workspaces
        std::fs::write(
            root.join("package.json"),
            r#"{"workspaces": ["packages/*"]}"#,
        )
        .unwrap();

        let pkg_dir = root.join("packages").join("core");
        std::fs::create_dir_all(&pkg_dir).unwrap();
        std::fs::write(pkg_dir.join("package.json"), r#"{"name": "@app/core"}"#).unwrap();

        // tsconfig with path alias
        std::fs::write(
            root.join("tsconfig.json"),
            r#"{"compilerOptions": {"paths": {"@/*": ["src/*"]}}}"#,
        )
        .unwrap();

        let ctx = discover_workspace_context(root);
        assert_eq!(ctx.packages.len(), 1);
        assert_eq!(ctx.packages[0].name, "@app/core");
        assert_eq!(ctx.aliases.len(), 1);
        assert_eq!(ctx.aliases[0].prefix, "@/");
    }
}
