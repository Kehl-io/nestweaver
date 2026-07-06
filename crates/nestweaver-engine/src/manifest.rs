use std::collections::HashMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::content_reader::ContentReader;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ManifestInfo {
    pub package_name: Option<String>,
    pub dependencies: Vec<String>,
    /// File paths referenced by `main`, `bin`, and `exports` in package.json.
    /// These are entry points for the package and their symbols should not be
    /// flagged as dead code.
    #[serde(default)]
    pub entry_files: Vec<String>,
}

/// Parse the manifest file(s) found in `repo_path` and return extracted
/// package name and dependency list. The first recognized manifest format wins.
pub fn parse_manifest(reader: &dyn ContentReader) -> ManifestInfo {
    if let Some(info) = parse_package_json(reader) {
        return info;
    }
    if let Some(info) = parse_go_mod(reader) {
        return info;
    }
    if let Some(info) = parse_cargo_toml(reader) {
        return info;
    }
    if let Some(info) = parse_pyproject_toml(reader) {
        return info;
    }
    if let Some(info) = parse_requirements_txt(reader) {
        return info;
    }
    if let Some(info) = parse_composer_json(reader) {
        return info;
    }
    if let Some(info) = parse_gemfile(reader) {
        return info;
    }
    if let Some(info) = parse_pubspec_yaml(reader) {
        return info;
    }
    if let Some(info) = parse_package_swift(reader) {
        return info;
    }
    if let Some(info) = parse_csproj(reader) {
        return info;
    }
    if let Some(info) = parse_build_gradle_kts(reader) {
        return info;
    }
    if let Some(info) = parse_cmake(reader) {
        return info;
    }
    ManifestInfo::default()
}

/// Persist a `HashMap<repo_uid, ManifestInfo>` as a JSON sidecar file.
pub fn save_manifest_cache(
    manifests: &HashMap<String, ManifestInfo>,
    path: &Path,
) -> Result<(), anyhow::Error> {
    let json = serde_json::to_string(manifests)?;
    std::fs::write(path, json)?;
    Ok(())
}

/// Load a `HashMap<repo_uid, ManifestInfo>` from a JSON sidecar file.
///
/// Returns an empty map when the file does not exist.
pub fn load_manifest_cache(path: &Path) -> Result<HashMap<String, ManifestInfo>, anyhow::Error> {
    if !path.exists() {
        return Ok(HashMap::new());
    }
    let json = std::fs::read_to_string(path)?;
    let map = serde_json::from_str(&json)?;
    Ok(map)
}

// ── per-format parsers ────────────────────────────────────────────────────────

fn parse_package_json(reader: &dyn ContentReader) -> Option<ManifestInfo> {
    let content = reader.read_file(Path::new("package.json")).ok()?;
    let json: serde_json::Value = serde_json::from_str(&content).ok()?;

    let package_name = json.get("name")?.as_str().map(String::from);

    let mut deps = Vec::new();
    for field in ["dependencies", "devDependencies", "peerDependencies"] {
        if let Some(obj) = json.get(field).and_then(|v| v.as_object()) {
            deps.extend(obj.keys().cloned());
        }
    }

    // Extract entry files from main, bin, and exports fields.
    let mut entry_files = Vec::new();
    if let Some(main) = json.get("main").and_then(|v| v.as_str()) {
        entry_files.push(main.to_string());
    }
    if let Some(bin) = json.get("bin") {
        match bin {
            serde_json::Value::String(s) => entry_files.push(s.clone()),
            serde_json::Value::Object(obj) => {
                for v in obj.values() {
                    if let Some(s) = v.as_str() {
                        entry_files.push(s.to_string());
                    }
                }
            }
            _ => {}
        }
    }
    if let Some(exports) = json.get("exports") {
        collect_export_paths(exports, &mut entry_files);
    }

    Some(ManifestInfo {
        package_name,
        dependencies: deps,
        entry_files,
    })
}

/// Recursively collect string values from the `exports` field of package.json.
/// The `exports` field can be a string, an object with condition keys mapping
/// to strings or nested objects, or an object with subpath keys.
fn collect_export_paths(value: &serde_json::Value, out: &mut Vec<String>) {
    match value {
        serde_json::Value::String(s) => out.push(s.clone()),
        serde_json::Value::Object(obj) => {
            for v in obj.values() {
                collect_export_paths(v, out);
            }
        }
        _ => {}
    }
}

fn parse_go_mod(reader: &dyn ContentReader) -> Option<ManifestInfo> {
    let content = reader.read_file(Path::new("go.mod")).ok()?;

    let mut package_name = None;
    let mut deps = Vec::new();
    let mut in_require = false;

    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("module ") {
            package_name = Some(trimmed.strip_prefix("module ")?.trim().to_string());
        } else if trimmed == "require (" {
            in_require = true;
        } else if trimmed == ")" {
            in_require = false;
        } else if in_require && !trimmed.is_empty() && !trimmed.starts_with("//") {
            // "github.com/pkg/errors v0.9.1" or "… // indirect"
            let module_path = trimmed.split_whitespace().next()?;
            deps.push(module_path.to_string());
        } else if trimmed.starts_with("require ") && !trimmed.contains('(') {
            // Single-line require: "require github.com/pkg/errors v0.9.1"
            let parts: Vec<&str> = trimmed.split_whitespace().collect();
            if parts.len() >= 2 {
                deps.push(parts[1].to_string());
            }
        }
    }

    Some(ManifestInfo {
        package_name,
        dependencies: deps,
        entry_files: vec![],
    })
}

fn parse_cargo_toml(reader: &dyn ContentReader) -> Option<ManifestInfo> {
    let content = reader.read_file(Path::new("Cargo.toml")).ok()?;
    let toml: toml::Value = toml::from_str(&content).ok()?;

    let package_name = toml.get("package")?.get("name")?.as_str().map(String::from);

    let mut deps = Vec::new();
    for section in ["dependencies", "dev-dependencies", "build-dependencies"] {
        if let Some(table) = toml.get(section).and_then(|v| v.as_table()) {
            deps.extend(table.keys().cloned());
        }
    }

    Some(ManifestInfo {
        package_name,
        dependencies: deps,
        entry_files: vec![],
    })
}

fn parse_pyproject_toml(reader: &dyn ContentReader) -> Option<ManifestInfo> {
    let content = reader.read_file(Path::new("pyproject.toml")).ok()?;
    let toml: toml::Value = toml::from_str(&content).ok()?;

    let package_name = toml.get("project")?.get("name")?.as_str().map(String::from);

    let mut deps = Vec::new();
    if let Some(dep_list) = toml
        .get("project")
        .and_then(|p| p.get("dependencies"))
        .and_then(|d| d.as_array())
    {
        for dep in dep_list {
            if let Some(s) = dep.as_str() {
                // PEP 508: "package-name>=1.0" — extract name before version specifier
                let name = s
                    .split(|c: char| !c.is_alphanumeric() && c != '-' && c != '_' && c != '.')
                    .next()
                    .unwrap_or(s);
                deps.push(name.to_string());
            }
        }
    }

    Some(ManifestInfo {
        package_name,
        dependencies: deps,
        entry_files: vec![],
    })
}

fn parse_requirements_txt(reader: &dyn ContentReader) -> Option<ManifestInfo> {
    let content = reader.read_file(Path::new("requirements.txt")).ok()?;

    let mut deps = Vec::new();
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') || trimmed.starts_with('-') {
            continue;
        }
        let name = trimmed
            .split(|c: char| !c.is_alphanumeric() && c != '-' && c != '_' && c != '.')
            .next()
            .unwrap_or(trimmed);
        if !name.is_empty() {
            deps.push(name.to_string());
        }
    }

    if deps.is_empty() {
        return None;
    }
    Some(ManifestInfo {
        package_name: None,
        dependencies: deps,
        entry_files: vec![],
    })
}

fn parse_composer_json(reader: &dyn ContentReader) -> Option<ManifestInfo> {
    let content = reader.read_file(Path::new("composer.json")).ok()?;
    let json: serde_json::Value = serde_json::from_str(&content).ok()?;

    let package_name = json.get("name").and_then(|v| v.as_str()).map(String::from);

    let mut deps = Vec::new();
    for field in ["require", "require-dev"] {
        if let Some(obj) = json.get(field).and_then(|v| v.as_object()) {
            for key in obj.keys() {
                if key != "php" && !key.starts_with("ext-") {
                    deps.push(key.clone());
                }
            }
        }
    }

    Some(ManifestInfo {
        package_name,
        dependencies: deps,
        entry_files: vec![],
    })
}

fn parse_gemfile(reader: &dyn ContentReader) -> Option<ManifestInfo> {
    let content = reader.read_file(Path::new("Gemfile")).ok()?;

    let mut deps = Vec::new();
    for line in content.lines() {
        let trimmed = line.trim();
        if let Some(after) = trimmed
            .strip_prefix("gem ")
            .or_else(|| trimmed.strip_prefix("gem("))
        {
            let name = after
                .trim_start_matches(['\'', '"'])
                .split(['\'', '"'])
                .next()
                .unwrap_or("");
            if !name.is_empty() {
                deps.push(name.to_string());
            }
        }
    }

    if deps.is_empty() {
        return None;
    }
    Some(ManifestInfo {
        package_name: None,
        dependencies: deps,
        entry_files: vec![],
    })
}

fn parse_pubspec_yaml(reader: &dyn ContentReader) -> Option<ManifestInfo> {
    let content = reader.read_file(Path::new("pubspec.yaml")).ok()?;
    let yaml: serde_yaml::Value = serde_yaml::from_str(&content).ok()?;

    let package_name = yaml.get("name").and_then(|v| v.as_str()).map(String::from);

    let mut deps = Vec::new();
    for field in ["dependencies", "dev_dependencies"] {
        if let Some(mapping) = yaml.get(field).and_then(|v| v.as_mapping()) {
            for key in mapping.keys() {
                if let Some(name) = key
                    .as_str()
                    .filter(|n| *n != "flutter" && *n != "flutter_test")
                {
                    deps.push(name.to_string());
                }
            }
        }
    }

    Some(ManifestInfo {
        package_name,
        dependencies: deps,
        entry_files: vec![],
    })
}

fn parse_package_swift(reader: &dyn ContentReader) -> Option<ManifestInfo> {
    let content = reader.read_file(Path::new("Package.swift")).ok()?;

    let package_name = content.lines().find_map(|line| {
        let trimmed = line.trim();
        if let Some(after) = trimmed.strip_prefix("name:") {
            let name = after
                .trim()
                .trim_matches(|c: char| c == '"' || c == ',' || c == ' ');
            if !name.is_empty() {
                return Some(name.to_string());
            }
        }
        None
    });

    let mut deps = Vec::new();
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.contains(".package(url:")
            && let Some(url_start) = trimmed.find("url:")
        {
            let after_url = &trimmed[url_start + 4..];
            let url = after_url
                .trim()
                .trim_start_matches([' ', '"'])
                .split('"')
                .next()
                .unwrap_or("");
            if let Some(last_segment) = url.rsplit('/').next() {
                let name = last_segment.trim_end_matches(".git");
                if !name.is_empty() {
                    deps.push(name.to_string());
                }
            }
        }
    }

    Some(ManifestInfo {
        package_name,
        dependencies: deps,
        entry_files: vec![],
    })
}

/// Find the first `.csproj` file in the repo root or one level of
/// subdirectories, using the reader's file listing instead of `read_dir`.
fn find_csproj(reader: &dyn ContentReader) -> Option<std::path::PathBuf> {
    let files = reader.list_files().ok()?;
    // Prefer root-level csproj files, then one-level subdirectory files.
    let mut root_level: Option<std::path::PathBuf> = None;
    let mut subdir_level: Option<std::path::PathBuf> = None;
    for f in &files {
        if f.extension().is_some_and(|ext| ext == "csproj") {
            let depth = f.components().count();
            if depth == 1 && root_level.is_none() {
                root_level = Some(f.clone());
            } else if depth == 2 && subdir_level.is_none() {
                subdir_level = Some(f.clone());
            }
        }
    }
    root_level.or(subdir_level)
}

fn parse_csproj(reader: &dyn ContentReader) -> Option<ManifestInfo> {
    let csproj_rel = find_csproj(reader)?;
    let content = reader.read_file(&csproj_rel).ok()?;

    let mut deps = Vec::new();
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.contains("PackageReference")
            && let Some(start) = trimmed.find("Include=\"")
        {
            let after = &trimmed[start + 9..];
            if let Some(end) = after.find('"') {
                let name = &after[..end];
                if !name.is_empty() {
                    deps.push(name.to_string());
                }
            }
        }
    }

    if deps.is_empty() {
        return None;
    }
    Some(ManifestInfo {
        package_name: None,
        dependencies: deps,
        entry_files: vec![],
    })
}

fn parse_build_gradle_kts(reader: &dyn ContentReader) -> Option<ManifestInfo> {
    let content = reader.read_file(Path::new("build.gradle.kts")).ok()?;

    let mut deps = Vec::new();
    for line in content.lines() {
        let trimmed = line.trim();
        for prefix in [
            "implementation(\"",
            "api(\"",
            "testImplementation(\"",
            "runtimeOnly(\"",
            "compileOnly(\"",
            "implementation('",
            "api('",
            "testImplementation('",
            "runtimeOnly('",
            "compileOnly('",
        ] {
            if let Some(after) = trimmed.strip_prefix(prefix) {
                let dep_str = after.split(['"', '\'']).next().unwrap_or("");
                let parts: Vec<&str> = dep_str.split(':').collect();
                if parts.len() >= 2 {
                    deps.push(format!("{}:{}", parts[0], parts[1]));
                }
            }
        }
    }

    if deps.is_empty() {
        return None;
    }
    Some(ManifestInfo {
        package_name: None,
        dependencies: deps,
        entry_files: vec![],
    })
}

fn parse_cmake(reader: &dyn ContentReader) -> Option<ManifestInfo> {
    let content = reader.read_file(Path::new("CMakeLists.txt")).ok()?;

    let mut package_name = None;
    let mut deps = Vec::new();

    for line in content.lines() {
        let trimmed = line.trim();
        if let Some(after) = trimmed.strip_prefix("project(") {
            let name = after.split([')', ' ']).next().unwrap_or("");
            if !name.is_empty() {
                package_name = Some(name.to_string());
            }
        }
        if let Some(after) = trimmed.strip_prefix("find_package(") {
            let name = after.split([')', ' ']).next().unwrap_or("");
            if !name.is_empty() {
                deps.push(name.to_string());
            }
        }
    }

    Some(ManifestInfo {
        package_name,
        dependencies: deps,
        entry_files: vec![],
    })
}

// ── tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::content_reader::FilesystemReader;

    #[test]
    fn parse_package_json_extracts_name_and_deps() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("package.json"),
            r#"{
                "name": "@myorg/api-client",
                "dependencies": { "axios": "^1.0.0", "@myorg/shared-types": "^2.0.0" },
                "devDependencies": { "jest": "^29.0.0" }
            }"#,
        )
        .unwrap();
        let info = parse_manifest(&FilesystemReader::new(dir.path()));
        assert_eq!(info.package_name.as_deref(), Some("@myorg/api-client"));
        assert!(info.dependencies.contains(&"axios".to_string()));
        assert!(
            info.dependencies
                .contains(&"@myorg/shared-types".to_string())
        );
        assert!(info.dependencies.contains(&"jest".to_string()));
    }

    #[test]
    fn parse_go_mod_extracts_module_and_requires() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("go.mod"),
            "module github.com/myorg/service\n\ngo 1.21\n\nrequire (\n\tgithub.com/myorg/shared v1.0.0\n\tgithub.com/pkg/errors v0.9.1\n)\n",
        )
        .unwrap();
        let info = parse_manifest(&FilesystemReader::new(dir.path()));
        assert_eq!(
            info.package_name.as_deref(),
            Some("github.com/myorg/service")
        );
        assert!(
            info.dependencies
                .contains(&"github.com/myorg/shared".to_string())
        );
    }

    #[test]
    fn parse_cargo_toml_extracts_name_and_deps() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("Cargo.toml"),
            r#"
[package]
name = "my-crate"
version = "0.1.0"

[dependencies]
serde = "1.0"
my-shared = { path = "../shared" }
"#,
        )
        .unwrap();
        let info = parse_manifest(&FilesystemReader::new(dir.path()));
        assert_eq!(info.package_name.as_deref(), Some("my-crate"));
        assert!(info.dependencies.contains(&"serde".to_string()));
        assert!(info.dependencies.contains(&"my-shared".to_string()));
    }

    #[test]
    fn parse_returns_default_for_no_manifest() {
        let dir = tempfile::tempdir().unwrap();
        let info = parse_manifest(&FilesystemReader::new(dir.path()));
        assert!(info.package_name.is_none());
        assert!(info.dependencies.is_empty());
    }

    #[test]
    fn parse_pyproject_extracts_name_and_deps() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("pyproject.toml"),
            r#"
[project]
name = "myservice"
dependencies = ["requests>=2.28", "pydantic>=2.0"]
"#,
        )
        .unwrap();
        let info = parse_manifest(&FilesystemReader::new(dir.path()));
        assert_eq!(info.package_name.as_deref(), Some("myservice"));
        assert!(info.dependencies.contains(&"requests".to_string()));
        assert!(info.dependencies.contains(&"pydantic".to_string()));
    }

    #[test]
    fn parse_requirements_txt_extracts_deps() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("requirements.txt"),
            "# comment\nrequests==2.28.0\npydantic>=2.0\n-r other.txt\n",
        )
        .unwrap();
        let info = parse_manifest(&FilesystemReader::new(dir.path()));
        assert!(info.package_name.is_none());
        assert!(info.dependencies.contains(&"requests".to_string()));
        assert!(info.dependencies.contains(&"pydantic".to_string()));
    }

    #[test]
    fn save_and_load_manifest_cache_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let cache_path = dir.path().join("test.manifests.json");

        let mut cache = HashMap::new();
        cache.insert(
            "r1".to_string(),
            ManifestInfo {
                package_name: Some("my-pkg".to_string()),
                dependencies: vec!["dep-a".to_string()],
                entry_files: vec![],
            },
        );
        save_manifest_cache(&cache, &cache_path).unwrap();

        let loaded = load_manifest_cache(&cache_path).unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded["r1"].package_name.as_deref(), Some("my-pkg"));
        assert!(loaded["r1"].dependencies.contains(&"dep-a".to_string()));
    }

    #[test]
    fn load_manifest_cache_returns_empty_when_missing() {
        let dir = tempfile::tempdir().unwrap();
        let cache_path = dir.path().join("nonexistent.json");
        let loaded = load_manifest_cache(&cache_path).unwrap();
        assert!(loaded.is_empty());
    }

    #[test]
    fn parse_composer_json_extracts_name_and_deps() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("composer.json"),
            r#"{"name":"myorg/api","require":{"laravel/framework":"^10.0"},"require-dev":{"phpunit/phpunit":"^10.0"}}"#,
        )
        .unwrap();
        let info = parse_manifest(&FilesystemReader::new(dir.path()));
        assert_eq!(info.package_name.as_deref(), Some("myorg/api"));
        assert!(info.dependencies.contains(&"laravel/framework".to_string()));
        assert!(info.dependencies.contains(&"phpunit/phpunit".to_string()));
    }

    #[test]
    fn parse_gemfile_extracts_deps() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("Gemfile"),
            "source 'https://rubygems.org'\n\ngem 'rails', '~> 7.0'\ngem 'pg'\n",
        )
        .unwrap();
        let info = parse_manifest(&FilesystemReader::new(dir.path()));
        assert!(info.dependencies.contains(&"rails".to_string()));
        assert!(info.dependencies.contains(&"pg".to_string()));
    }

    #[test]
    fn parse_pubspec_yaml_extracts_name_and_deps() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("pubspec.yaml"),
            "name: my_app\ndependencies:\n  http: ^0.13.0\n",
        )
        .unwrap();
        let info = parse_manifest(&FilesystemReader::new(dir.path()));
        assert_eq!(info.package_name.as_deref(), Some("my_app"));
        assert!(info.dependencies.contains(&"http".to_string()));
    }

    #[test]
    fn parse_package_swift_extracts_deps() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("Package.swift"),
            "import PackageDescription\nlet package = Package(\n    name: \"MyPkg\",\n    dependencies: [\n        .package(url: \"https://github.com/apple/swift-argument-parser.git\", from: \"1.0.0\"),\n    ]\n)\n",
        )
        .unwrap();
        let info = parse_manifest(&FilesystemReader::new(dir.path()));
        assert_eq!(info.package_name.as_deref(), Some("MyPkg"));
        assert!(
            info.dependencies
                .contains(&"swift-argument-parser".to_string())
        );
    }

    #[test]
    fn parse_csproj_extracts_deps() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("MyApp.csproj"),
            "<Project Sdk=\"Microsoft.NET.Sdk\">\n  <ItemGroup>\n    <PackageReference Include=\"Newtonsoft.Json\" Version=\"13.0.1\" />\n  </ItemGroup>\n</Project>",
        )
        .unwrap();
        let info = parse_manifest(&FilesystemReader::new(dir.path()));
        assert!(info.dependencies.contains(&"Newtonsoft.Json".to_string()));
    }

    #[test]
    fn parse_build_gradle_kts_extracts_deps() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("build.gradle.kts"),
            "dependencies {\n    implementation(\"org.springframework.boot:spring-boot-starter-web:3.1.0\")\n}\n",
        )
        .unwrap();
        let info = parse_manifest(&FilesystemReader::new(dir.path()));
        assert!(
            info.dependencies
                .contains(&"org.springframework.boot:spring-boot-starter-web".to_string())
        );
    }

    #[test]
    fn parse_cmake_extracts_name_and_deps() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("CMakeLists.txt"),
            "cmake_minimum_required(VERSION 3.20)\nproject(MyApp)\nfind_package(Boost REQUIRED)\nfind_package(OpenSSL REQUIRED)\n",
        )
        .unwrap();
        let info = parse_manifest(&FilesystemReader::new(dir.path()));
        assert_eq!(info.package_name.as_deref(), Some("MyApp"));
        assert!(info.dependencies.contains(&"Boost".to_string()));
        assert!(info.dependencies.contains(&"OpenSSL".to_string()));
    }

    #[test]
    fn parse_package_json_extracts_entry_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("package.json"),
            r#"{
                "name": "my-lib",
                "main": "./dist/index.js",
                "bin": {
                    "cli": "./bin/cli.js"
                },
                "exports": {
                    ".": {
                        "import": "./dist/esm/index.js",
                        "require": "./dist/cjs/index.js"
                    },
                    "./utils": "./dist/utils.js"
                },
                "dependencies": {}
            }"#,
        )
        .unwrap();
        let info = parse_manifest(&FilesystemReader::new(dir.path()));
        assert!(info.entry_files.contains(&"./dist/index.js".to_string()));
        assert!(info.entry_files.contains(&"./bin/cli.js".to_string()));
        assert!(
            info.entry_files
                .contains(&"./dist/esm/index.js".to_string())
        );
        assert!(
            info.entry_files
                .contains(&"./dist/cjs/index.js".to_string())
        );
        assert!(info.entry_files.contains(&"./dist/utils.js".to_string()));
    }

    #[test]
    fn parse_package_json_bin_as_string() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("package.json"),
            r#"{
                "name": "my-cli",
                "bin": "./bin/main.js",
                "dependencies": {}
            }"#,
        )
        .unwrap();
        let info = parse_manifest(&FilesystemReader::new(dir.path()));
        assert!(info.entry_files.contains(&"./bin/main.js".to_string()));
    }

    #[test]
    fn parse_package_json_no_entry_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("package.json"),
            r#"{
                "name": "simple-pkg",
                "dependencies": { "lodash": "^4.0.0" }
            }"#,
        )
        .unwrap();
        let info = parse_manifest(&FilesystemReader::new(dir.path()));
        assert!(info.entry_files.is_empty());
    }
}
