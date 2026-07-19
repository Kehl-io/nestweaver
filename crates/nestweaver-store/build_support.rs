use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Debug, PartialEq, Eq)]
pub struct ResolvedDependency {
    pub package_id: String,
    pub version: String,
    pub source: Option<String>,
    pub manifest_path: PathBuf,
    pub source_dir: PathBuf,
}

#[derive(Debug)]
pub struct CargoMetadataResolution {
    pub json: String,
    pub resolution_manifest: PathBuf,
    pub lockfile: PathBuf,
    pub used_isolated_resolver: bool,
}

pub fn locate_project_arguments(manifest_path: &Path) -> Vec<OsString> {
    vec![
        OsString::from("locate-project"),
        OsString::from("--workspace"),
        OsString::from("--message-format"),
        OsString::from("plain"),
        OsString::from("--manifest-path"),
        manifest_path.as_os_str().to_owned(),
    ]
}

pub fn metadata_arguments(manifest_path: &Path, locked: bool) -> Vec<OsString> {
    let mut arguments = vec![
        OsString::from("metadata"),
        OsString::from("--format-version"),
        OsString::from("1"),
        OsString::from("--manifest-path"),
        manifest_path.as_os_str().to_owned(),
    ];
    if locked {
        arguments.push(OsString::from("--locked"));
    }
    arguments
}

pub fn cargo_metadata_for_dependency(
    cargo: OsString,
    package_manifest: &Path,
    out_dir: &Path,
    dependency_name: &str,
    exact_version: &str,
) -> Result<CargoMetadataResolution, String> {
    let locate_output = run_cargo(
        &cargo,
        &locate_project_arguments(package_manifest),
        package_manifest
            .parent()
            .ok_or_else(|| format!("{} has no parent directory", package_manifest.display()))?,
        "locate-project",
        package_manifest,
    )?;
    let workspace_manifest = PathBuf::from(
        String::from_utf8(locate_output)
            .map_err(|error| format!("cargo locate-project returned non-UTF-8 output: {error}"))?
            .trim(),
    );
    if !workspace_manifest.is_file() {
        return Err(format!(
            "cargo locate-project returned missing manifest {}",
            workspace_manifest.display()
        ));
    }

    let workspace_lock = workspace_manifest.with_file_name("Cargo.lock");
    if workspace_lock.is_file() {
        let json = run_metadata(&cargo, &workspace_manifest, true)?;
        return Ok(CargoMetadataResolution {
            json,
            resolution_manifest: package_manifest.to_path_buf(),
            lockfile: workspace_lock,
            used_isolated_resolver: false,
        });
    }

    let resolver_manifest =
        write_isolated_resolver_manifest(out_dir, dependency_name, exact_version)?;
    // The first metadata pass may create a lockfile, but only beside the
    // disposable resolver manifest under OUT_DIR. Never run this unlocked
    // against the package or vendored source manifest.
    run_metadata(&cargo, &resolver_manifest, false)?;
    let resolver_lock = resolver_manifest.with_file_name("Cargo.lock");
    if !resolver_lock.is_file() {
        return Err(format!(
            "cargo metadata did not create isolated resolver lockfile {}",
            resolver_lock.display()
        ));
    }
    let json = run_metadata(&cargo, &resolver_manifest, true)?;
    Ok(CargoMetadataResolution {
        json,
        resolution_manifest: resolver_manifest,
        lockfile: resolver_lock,
        used_isolated_resolver: true,
    })
}

pub fn write_isolated_resolver_manifest(
    out_dir: &Path,
    dependency_name: &str,
    exact_version: &str,
) -> Result<PathBuf, String> {
    if dependency_name.is_empty()
        || !dependency_name
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'))
    {
        return Err(format!(
            "dependency name {dependency_name:?} is not safe for an isolated Cargo manifest"
        ));
    }
    if exact_version.is_empty()
        || !exact_version.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '.' | '-' | '+')
        })
    {
        return Err(format!(
            "dependency version {exact_version:?} is not safe for an isolated Cargo manifest"
        ));
    }

    let resolver_dir = out_dir.join("nestweaver-dependency-resolver");
    std::fs::create_dir_all(&resolver_dir).map_err(|error| {
        format!(
            "could not create isolated resolver directory {}: {error}",
            resolver_dir.display()
        )
    })?;
    let manifest_path = resolver_dir.join("Cargo.toml");
    let contents = format!(
        "[package]\nname = \"nestweaver-build-resolver\"\nversion = \"0.0.0\"\nedition = \"2021\"\npublish = false\n\n[workspace]\n\n[lib]\npath = \"lib.rs\"\n\n[dependencies]\n{dependency_name} = \"={exact_version}\"\n"
    );
    write_if_changed(&manifest_path, contents.as_bytes())?;
    write_if_changed(
        &resolver_dir.join("lib.rs"),
        b"// Isolated Cargo metadata resolver target.\n",
    )?;
    Ok(manifest_path)
}

pub fn validate_source_manifest_override(
    manifest_path: &Path,
    expected_name: &str,
    expected_version: &str,
    bundled_directory: &str,
) -> Result<PathBuf, String> {
    let contents = std::fs::read_to_string(manifest_path).map_err(|error| {
        format!(
            "could not read source manifest override {}: {error}",
            manifest_path.display()
        )
    })?;
    let manifest: toml::Value = toml::from_str(&contents).map_err(|error| {
        format!(
            "could not parse source manifest override {}: {error}",
            manifest_path.display()
        )
    })?;
    let package = manifest
        .get("package")
        .and_then(toml::Value::as_table)
        .ok_or_else(|| {
            format!(
                "source manifest override {} has no [package] table",
                manifest_path.display()
            )
        })?;
    let name = package
        .get("name")
        .and_then(toml::Value::as_str)
        .unwrap_or("<missing>");
    let version = package
        .get("version")
        .and_then(toml::Value::as_str)
        .unwrap_or("<missing>");
    if name != expected_name || version != expected_version {
        return Err(format!(
            "source manifest override {} identifies package {name} {version}; expected package {expected_name} {expected_version}",
            manifest_path.display()
        ));
    }
    let source_dir = manifest_path
        .parent()
        .ok_or_else(|| format!("{} has no parent directory", manifest_path.display()))?
        .join(bundled_directory);
    if !source_dir.is_dir() {
        return Err(format!(
            "source manifest override {} expects bundled source directory {}, but it does not exist",
            manifest_path.display(),
            source_dir.display()
        ));
    }
    Ok(source_dir)
}

fn run_metadata(cargo: &OsString, manifest_path: &Path, locked: bool) -> Result<String, String> {
    let output = run_cargo(
        cargo,
        &metadata_arguments(manifest_path, locked),
        manifest_path
            .parent()
            .ok_or_else(|| format!("{} has no parent directory", manifest_path.display()))?,
        "metadata",
        manifest_path,
    )?;
    String::from_utf8(output)
        .map_err(|error| format!("cargo metadata returned non-UTF-8 JSON: {error}"))
}

fn run_cargo(
    cargo: &OsString,
    arguments: &[OsString],
    current_dir: &Path,
    operation: &str,
    manifest_path: &Path,
) -> Result<Vec<u8>, String> {
    let output = Command::new(cargo)
        .args(arguments)
        .current_dir(current_dir)
        .output()
        .map_err(|error| {
            format!(
                "failed to execute {} {operation} for {}: {error}",
                Path::new(cargo).display(),
                manifest_path.display()
            )
        })?;
    if !output.status.success() {
        return Err(format!(
            "{} {operation} failed for {}:\n{}",
            Path::new(cargo).display(),
            manifest_path.display(),
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    Ok(output.stdout)
}

fn write_if_changed(path: &Path, contents: &[u8]) -> Result<(), String> {
    if std::fs::read(path).is_ok_and(|existing| existing == contents) {
        return Ok(());
    }
    std::fs::write(path, contents)
        .map_err(|error| format!("could not write {}: {error}", path.display()))
}

pub fn resolved_dependency_source(
    metadata_json: &str,
    package_manifest: &Path,
    dependency_name: &str,
    bundled_directory: &str,
) -> Result<ResolvedDependency, String> {
    let metadata: serde_json::Value = serde_json::from_str(metadata_json)
        .map_err(|error| format!("could not parse cargo metadata JSON: {error}"))?;
    let packages = metadata
        .get("packages")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| "cargo metadata JSON has no packages array".to_string())?;

    let current_packages: Vec<_> = packages
        .iter()
        .filter(|package| {
            package
                .get("manifest_path")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|path| paths_refer_to_same_file(Path::new(path), package_manifest))
        })
        .collect();
    let current_package = match current_packages.as_slice() {
        [package] => *package,
        [] => {
            return Err(format!(
                "cargo metadata has no package for manifest {}",
                package_manifest.display()
            ));
        }
        packages => {
            return Err(format!(
                "cargo metadata has {} packages for manifest {}",
                packages.len(),
                package_manifest.display()
            ));
        }
    };
    let current_id = string_field(current_package, "id")?;

    let nodes = metadata
        .pointer("/resolve/nodes")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| "cargo metadata JSON has no resolve.nodes array".to_string())?;
    let current_nodes: Vec<_> = nodes
        .iter()
        .filter(|node| node.get("id").and_then(serde_json::Value::as_str) == Some(current_id))
        .collect();
    let current_node = match current_nodes.as_slice() {
        [node] => *node,
        [] => {
            return Err(format!(
                "cargo metadata resolve has no node for {current_id}"
            ));
        }
        nodes => {
            return Err(format!(
                "cargo metadata resolve has {} nodes for {current_id}",
                nodes.len()
            ));
        }
    };
    let deps = current_node
        .get("deps")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| format!("cargo metadata resolve node {current_id} has no deps array"))?;
    let direct_dependencies: Vec<_> = deps
        .iter()
        .filter(|dependency| {
            dependency.get("name").and_then(serde_json::Value::as_str) == Some(dependency_name)
        })
        .collect();
    let direct_dependency = match direct_dependencies.as_slice() {
        [dependency] => *dependency,
        [] => {
            return Err(format!(
                "package {current_id} has no resolved direct dependency named {dependency_name}"
            ));
        }
        dependencies => {
            let identities = dependencies
                .iter()
                .filter_map(|dependency| dependency.get("pkg").and_then(serde_json::Value::as_str))
                .collect::<Vec<_>>()
                .join(", ");
            return Err(format!(
                "ambiguous direct dependency {dependency_name} for {current_id}: {identities}"
            ));
        }
    };
    let dependency_id = string_field(direct_dependency, "pkg")?;

    let dependency_packages: Vec<_> = packages
        .iter()
        .filter(|package| {
            package.get("id").and_then(serde_json::Value::as_str) == Some(dependency_id)
        })
        .collect();
    let dependency_package = match dependency_packages.as_slice() {
        [package] => *package,
        [] => {
            return Err(format!(
                "resolved direct dependency {dependency_id} has no package record"
            ));
        }
        packages => {
            return Err(format!(
                "resolved direct dependency {dependency_id} has multiple package records ({})",
                packages.len()
            ));
        }
    };
    let resolved_name = string_field(dependency_package, "name")?;
    if resolved_name != dependency_name {
        return Err(format!(
            "resolved dependency edge {dependency_name} points to package {dependency_id} named {resolved_name}"
        ));
    }
    let version = string_field(dependency_package, "version")?.to_string();
    let source = dependency_package
        .get("source")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string);
    let manifest_path = PathBuf::from(string_field(dependency_package, "manifest_path")?);
    let source_dir = manifest_path
        .parent()
        .ok_or_else(|| {
            format!(
                "resolved manifest {} has no parent",
                manifest_path.display()
            )
        })?
        .join(bundled_directory);
    if !source_dir.is_dir() {
        return Err(format!(
            "resolved dependency {dependency_id} expects bundled source directory {}, but it does not exist",
            source_dir.display()
        ));
    }

    Ok(ResolvedDependency {
        package_id: dependency_id.to_string(),
        version,
        source,
        manifest_path,
        source_dir,
    })
}

fn string_field<'a>(value: &'a serde_json::Value, field: &str) -> Result<&'a str, String> {
    value
        .get(field)
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| format!("cargo metadata value has no string field {field}: {value}"))
}

fn paths_refer_to_same_file(left: &Path, right: &Path) -> bool {
    if left == right {
        return true;
    }
    match (left.canonicalize(), right.canonicalize()) {
        (Ok(left), Ok(right)) => left == right,
        _ => false,
    }
}
