#[path = "../build_support.rs"]
mod build_support;

use serde_json::json;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

const LBUG_VERSION: &str = "0.19.1";

#[test]
fn resolved_edge_uses_cargo_identity_when_exact_version_exists_across_sources() {
    let fixture = Fixture::new();
    let crates_io = fixture.lbug_package(
        "registry+crates.io#lbug@0.19.1",
        "registry-a",
        LBUG_VERSION,
        true,
    );
    let mirror = fixture.lbug_package(
        "registry+mirror#lbug@0.19.1",
        "registry-b",
        LBUG_VERSION,
        true,
    );
    let metadata = fixture.metadata(
        vec![crates_io.clone(), mirror],
        vec![dep("lbug", "registry+crates.io#lbug@0.19.1")],
    );

    let selected = build_support::resolved_dependency_source(
        &metadata,
        &fixture.store_manifest,
        "lbug",
        "lbug-src",
    )
    .unwrap();

    assert_eq!(selected.package_id, "registry+crates.io#lbug@0.19.1");
    assert_eq!(selected.version, LBUG_VERSION);
    assert_eq!(selected.source.as_deref(), Some("registry+crates.io"));
    assert_eq!(selected.source_dir, fixture.source_dir(&crates_io));
}

#[test]
fn resolved_edge_selects_the_direct_dependency_when_multiple_versions_exist() {
    let fixture = Fixture::new();
    let old = fixture.lbug_package("registry+crates.io#lbug@0.16.1", "old", "0.16.1", true);
    let current = fixture.lbug_package(
        "registry+crates.io#lbug@0.19.1",
        "current",
        LBUG_VERSION,
        true,
    );
    let metadata = fixture.metadata(
        vec![old, current.clone()],
        vec![dep("lbug", "registry+crates.io#lbug@0.19.1")],
    );

    let selected = build_support::resolved_dependency_source(
        &metadata,
        &fixture.store_manifest,
        "lbug",
        "lbug-src",
    )
    .unwrap();

    assert_eq!(selected.version, LBUG_VERSION);
    assert_eq!(selected.source_dir, fixture.source_dir(&current));
}

#[test]
fn duplicate_direct_dependency_edges_are_rejected_as_ambiguous() {
    let fixture = Fixture::new();
    let first = fixture.lbug_package("registry+a#lbug@0.19.1", "a", LBUG_VERSION, true);
    let second = fixture.lbug_package("registry+b#lbug@0.19.1", "b", LBUG_VERSION, true);
    let metadata = fixture.metadata(
        vec![first, second],
        vec![
            dep("lbug", "registry+a#lbug@0.19.1"),
            dep("lbug", "registry+b#lbug@0.19.1"),
        ],
    );

    let error = build_support::resolved_dependency_source(
        &metadata,
        &fixture.store_manifest,
        "lbug",
        "lbug-src",
    )
    .unwrap_err();

    assert!(
        error.contains("ambiguous direct dependency lbug"),
        "{error}"
    );
    assert!(error.contains("registry+a#lbug@0.19.1"), "{error}");
    assert!(error.contains("registry+b#lbug@0.19.1"), "{error}");
}

#[test]
fn duplicate_records_for_the_resolved_package_identity_are_rejected() {
    let fixture = Fixture::new();
    let package = fixture.lbug_package("registry+a#lbug@0.19.1", "a", LBUG_VERSION, true);
    let metadata = fixture.metadata(
        vec![package.clone(), package],
        vec![dep("lbug", "registry+a#lbug@0.19.1")],
    );

    let error = build_support::resolved_dependency_source(
        &metadata,
        &fixture.store_manifest,
        "lbug",
        "lbug-src",
    )
    .unwrap_err();

    assert!(error.contains("multiple package records"), "{error}");
}

#[test]
fn missing_resolved_package_source_is_reported() {
    let fixture = Fixture::new();
    let metadata = fixture.metadata(
        Vec::new(),
        vec![dep("lbug", "registry+missing#lbug@0.19.1")],
    );

    let error = build_support::resolved_dependency_source(
        &metadata,
        &fixture.store_manifest,
        "lbug",
        "lbug-src",
    )
    .unwrap_err();

    assert!(error.contains("registry+missing#lbug@0.19.1"), "{error}");
    assert!(error.contains("has no package record"), "{error}");
}

#[test]
fn missing_bundled_source_directory_is_reported() {
    let fixture = Fixture::new();
    let package = fixture.lbug_package("registry+a#lbug@0.19.1", "a", LBUG_VERSION, false);
    let metadata = fixture.metadata(vec![package], vec![dep("lbug", "registry+a#lbug@0.19.1")]);

    let error = build_support::resolved_dependency_source(
        &metadata,
        &fixture.store_manifest,
        "lbug",
        "lbug-src",
    )
    .unwrap_err();

    assert!(error.contains("lbug-src"), "{error}");
    assert!(error.contains("does not exist"), "{error}");
}

#[test]
fn crate_manifest_pins_lbug_to_the_exact_abi_version() {
    // Derive the expected pin from build.rs's LBUG_ABI_VERSION rather than
    // hardcoding it, so the manifest and the build-script guard cannot drift
    // apart on an upgrade (they previously had to be edited in lockstep, and
    // a mismatch only surfaces as a confusing build-script panic).
    let build_rs =
        std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("build.rs")).unwrap();
    let abi = build_rs
        .lines()
        .find_map(|l| {
            let l = l.trim();
            let rest = l.strip_prefix("const LBUG_ABI_VERSION: &str = \"")?;
            rest.split('"').next()
        })
        .expect("build.rs must declare LBUG_ABI_VERSION");

    let manifest =
        std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml")).unwrap();
    let expected = format!("lbug = \"={abi}\"");
    assert!(
        manifest.contains(&expected),
        "Cargo.toml must pin lbug to the build-script ABI version ({expected}):\n{manifest}"
    );
}

#[test]
fn read_only_lockless_package_resolves_in_out_dir_without_source_writes() {
    let temp = tempfile::tempdir().unwrap();
    let package_dir = temp.path().join("read-only-published-package");
    let manifest = package_dir.join("Cargo.toml");
    let out_dir = temp.path().join("writable-out");
    std::fs::create_dir_all(package_dir.join("src")).unwrap();
    std::fs::create_dir_all(&out_dir).unwrap();
    std::fs::write(
        &manifest,
        "[package]\nname='published-store'\nversion='2.4.0'\nedition='2021'\n",
    )
    .unwrap();
    std::fs::write(package_dir.join("src/lib.rs"), "pub fn fixture() {}\n").unwrap();
    let before = directory_entries(&package_dir);
    let read_only = ReadOnlyDirectory::new(&package_dir);

    let resolution = build_support::cargo_metadata_for_dependency(
        cargo_executable(),
        &manifest,
        &out_dir,
        "lbug",
        LBUG_VERSION,
    )
    .unwrap();
    let selected = build_support::resolved_dependency_source(
        &resolution.json,
        &resolution.resolution_manifest,
        "lbug",
        "lbug-src",
    )
    .unwrap();

    assert_eq!(selected.version, LBUG_VERSION);
    assert!(resolution.used_isolated_resolver);
    assert!(resolution.lockfile.starts_with(&out_dir));
    assert!(resolution.lockfile.is_file());
    assert!(!package_dir.join("Cargo.lock").exists());
    assert_eq!(directory_entries(&package_dir), before);

    drop(read_only);
}

#[test]
fn isolated_resolver_manifest_uses_an_exact_dependency() {
    let temp = tempfile::tempdir().unwrap();
    let manifest =
        build_support::write_isolated_resolver_manifest(temp.path(), "lbug", LBUG_VERSION).unwrap();
    let contents = std::fs::read_to_string(manifest).unwrap();

    assert!(contents.contains("lbug = \"=0.19.1\""), "{contents}");
    assert!(contents.contains("[workspace]"), "{contents}");
}

#[test]
fn cargo_locates_the_workspace_before_metadata_resolution() {
    let temp = tempfile::tempdir().unwrap();
    let manifest = temp.path().join("crates/store/Cargo.toml");
    std::fs::create_dir_all(manifest.parent().unwrap()).unwrap();
    std::fs::write(&manifest, "[package]\nname='store'\nversion='1.0.0'\n").unwrap();

    let locate_args = build_support::locate_project_arguments(&manifest);
    let metadata_args = build_support::metadata_arguments(&temp.path().join("Cargo.toml"), true);

    assert_eq!(
        locate_args,
        vec![
            "locate-project",
            "--workspace",
            "--message-format",
            "plain",
            "--manifest-path",
            manifest.to_str().unwrap(),
        ]
    );
    assert!(metadata_args.iter().any(|arg| arg == "--locked"));
}

#[test]
fn vendored_manifest_path_is_the_source_of_bundled_files() {
    let fixture = Fixture::new();
    let package = fixture.lbug_package(
        "path+file:///vendor/lbug#0.19.1",
        "vendor/lbug",
        LBUG_VERSION,
        true,
    );
    let metadata = fixture.metadata(
        vec![package.clone()],
        vec![dep("lbug", "path+file:///vendor/lbug#0.19.1")],
    );

    let selected = build_support::resolved_dependency_source(
        &metadata,
        &fixture.store_manifest,
        "lbug",
        "lbug-src",
    )
    .unwrap();

    assert_eq!(selected.source, None);
    assert_eq!(selected.source_dir, fixture.source_dir(&package));
}

#[test]
fn source_manifest_override_accepts_exact_lbug_package() {
    let temp = tempfile::tempdir().unwrap();
    let manifest = temp.path().join("vendor/lbug/Cargo.toml");
    std::fs::create_dir_all(manifest.parent().unwrap().join("lbug-src")).unwrap();
    std::fs::write(
        &manifest,
        "[package]\nname = \"lbug\"\nversion = \"0.19.1\"\n",
    )
    .unwrap();

    let source = build_support::validate_source_manifest_override(
        &manifest,
        "lbug",
        LBUG_VERSION,
        "lbug-src",
    )
    .unwrap();

    assert_eq!(source, manifest.parent().unwrap().join("lbug-src"));
}

#[test]
fn source_manifest_override_rejects_wrong_package_identity() {
    let temp = tempfile::tempdir().unwrap();
    let manifest = temp.path().join("vendor/not-lbug/Cargo.toml");
    std::fs::create_dir_all(manifest.parent().unwrap().join("lbug-src")).unwrap();
    std::fs::write(
        &manifest,
        "[package]\nname = \"lookalike\"\nversion = \"0.19.1\"\n",
    )
    .unwrap();

    let error = build_support::validate_source_manifest_override(
        &manifest,
        "lbug",
        LBUG_VERSION,
        "lbug-src",
    )
    .unwrap_err();

    assert!(error.contains("expected package lbug 0.19.1"), "{error}");
    assert!(error.contains("lookalike 0.19.1"), "{error}");
}

#[test]
fn source_manifest_override_rejects_wrong_abi_version() {
    let temp = tempfile::tempdir().unwrap();
    let manifest = temp.path().join("vendor/lbug/Cargo.toml");
    std::fs::create_dir_all(manifest.parent().unwrap().join("lbug-src")).unwrap();
    std::fs::write(
        &manifest,
        "[package]\nname = \"lbug\"\nversion = \"0.18.1\"\n",
    )
    .unwrap();

    let error = build_support::validate_source_manifest_override(
        &manifest,
        "lbug",
        LBUG_VERSION,
        "lbug-src",
    )
    .unwrap_err();

    assert!(error.contains("expected package lbug 0.19.1"), "{error}");
    assert!(error.contains("lbug 0.18.1"), "{error}");
}

struct Fixture {
    _temp: tempfile::TempDir,
    root: PathBuf,
    store_manifest: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().to_path_buf();
        let store_manifest = root.join("workspace/crates/store/Cargo.toml");
        std::fs::create_dir_all(store_manifest.parent().unwrap()).unwrap();
        std::fs::write(
            &store_manifest,
            "[package]\nname='store'\nversion='2.4.0'\n",
        )
        .unwrap();
        Self {
            _temp: temp,
            root,
            store_manifest,
        }
    }

    fn lbug_package(
        &self,
        id: &str,
        relative_dir: &str,
        version: &str,
        create_source: bool,
    ) -> serde_json::Value {
        let manifest = self.root.join(relative_dir).join("Cargo.toml");
        std::fs::create_dir_all(manifest.parent().unwrap()).unwrap();
        std::fs::write(
            &manifest,
            format!("[package]\nname='lbug'\nversion='{version}'\n"),
        )
        .unwrap();
        if create_source {
            std::fs::create_dir_all(manifest.parent().unwrap().join("lbug-src")).unwrap();
        }
        let source = id
            .strip_prefix("registry+")
            .and_then(|value| value.split('#').next());
        json!({
            "id": id,
            "name": "lbug",
            "version": version,
            "source": source.map(|value| format!("registry+{value}")),
            "manifest_path": manifest,
        })
    }

    fn source_dir(&self, package: &serde_json::Value) -> PathBuf {
        Path::new(package["manifest_path"].as_str().unwrap())
            .parent()
            .unwrap()
            .join("lbug-src")
    }

    fn metadata(
        &self,
        dependencies: Vec<serde_json::Value>,
        deps: Vec<serde_json::Value>,
    ) -> String {
        let store_id = "path+file:///workspace/crates/store#2.4.0";
        let mut packages = vec![json!({
            "id": store_id,
            "name": "nestweaver-store",
            "version": "2.4.0",
            "source": null,
            "manifest_path": self.store_manifest,
        })];
        packages.extend(dependencies);
        json!({
            "packages": packages,
            "resolve": {
                "nodes": [{
                    "id": store_id,
                    "deps": deps,
                }]
            }
        })
        .to_string()
    }
}

fn dep(name: &str, package_id: &str) -> serde_json::Value {
    json!({
        "name": name,
        "pkg": package_id,
        "dep_kinds": [{"kind": null, "target": null}],
    })
}

fn cargo_executable() -> OsString {
    std::env::var_os("CARGO").unwrap_or_else(|| OsString::from("cargo"))
}

fn directory_entries(path: &Path) -> Vec<PathBuf> {
    let mut entries = std::fs::read_dir(path)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect::<Vec<_>>();
    entries.sort();
    entries
}

struct ReadOnlyDirectory {
    path: PathBuf,
}

impl ReadOnlyDirectory {
    fn new(path: &Path) -> Self {
        set_tree_read_only(path, true);
        Self {
            path: path.to_path_buf(),
        }
    }
}

impl Drop for ReadOnlyDirectory {
    fn drop(&mut self) {
        set_tree_read_only(&self.path, false);
    }
}

fn set_tree_read_only(path: &Path, read_only: bool) {
    use std::os::unix::fs::PermissionsExt;

    if path.is_dir() && !read_only {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    if path.is_dir() {
        for entry in std::fs::read_dir(path).unwrap() {
            set_tree_read_only(&entry.unwrap().path(), read_only);
        }
    }
    let mode = match (path.is_dir(), read_only) {
        (true, true) => 0o555,
        (true, false) => 0o755,
        (false, true) => 0o444,
        (false, false) => 0o644,
    };
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
}
