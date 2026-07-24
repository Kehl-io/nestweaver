use std::collections::HashSet;

/// Resolve a Rust `use` path to a file path within the repo.
///
/// Rust use paths come in these forms:
/// - `use crate::foo::bar;`     — relative to the current crate's `src/` dir
/// - `use super::foo;`          — relative to the parent module
/// - `use self::foo;`           — relative to the current module
/// - `use other_crate::foo;`    — a sibling crate in the same cargo workspace
/// - `use my_crate::foo;`       — the crate under test from `tests/` (root crate)
///
/// Crate roots are inferred from the known file set: any `…/src/lib.rs` or
/// `…/src/main.rs` marks a crate whose name is the normalized directory
/// basename (`-` and `_` are interchangeable, matching cargo's package-name →
/// crate-name convention). An unmatched crate name first tries the
/// unique-crate fallback (the module path resolves under exactly one
/// workspace crate — covers crates whose directory differs from the package
/// name), then the repo-root crate (`src/lib.rs`/`src/main.rs`) as a last
/// resort. The root crate has no directory name, so in a multi-crate
/// workspace it is never a fallback candidate: matching an unmatched name
/// against it by module-path coincidence would steal member-crate imports and
/// bind unrelated external crates (e.g. `serde`) whose module segments happen
/// to exist under `src/`. In a single-crate repo it is used from dev-target
/// files only, and only when at least one module segment actually resolves —
/// this covers integration tests importing the crate under test without
/// matching unrelated external crates.
///
/// The final path segment usually names an item (function, struct) inside a
/// module rather than a module itself, so resolution returns the deepest
/// module file matched, falling back to the crate root file (`lib.rs`) when
/// the crate name matched but no module segment did (the item may be defined
/// in or re-exported from the crate root).
pub fn resolve_import(
    from_file: &str,
    specifier: &str,
    known_files: &HashSet<&str>,
) -> Option<String> {
    let segments: Vec<&str> = specifier.split("::").filter(|s| !s.is_empty()).collect();
    let (first, rest) = segments.split_first()?;

    match *first {
        "crate" => {
            let base = current_crate_src_dir(from_file, known_files)?;
            resolve_module_path(&base, rest, known_files)
                .or_else(|| crate_root_file(&base, known_files))
        }
        "self" => {
            let base = self_module_dir(from_file);
            resolve_module_path(&base, rest, known_files)
        }
        "super" => {
            let mut ups = 1usize;
            while rest.get(ups - 1).is_some_and(|s| *s == "super") {
                ups += 1;
            }
            let mut base = super_module_dir(from_file);
            for _ in 1..ups {
                base = parent_dir(&base).to_string();
            }
            resolve_module_path(&base, &rest[ups - 1..], known_files)
        }
        crate_name => {
            // Rust built-in libraries are never workspace crates — resolve
            // them to nothing BEFORE any fallback can bind them to a local
            // module (`use std::fs` must not edge to `src/fs.rs`, P2 review).
            // Third-party crates (tokio, serde, …) still rely on the
            // ambiguity guards below; full precision needs package-name
            // evidence from Cargo.toml (not yet threaded into the resolver).
            if matches!(crate_name, "std" | "core" | "alloc" | "proc_macro" | "test") {
                return None;
            }
            // Named crate: find a workspace crate whose directory basename
            // matches (cargo normalizes `-` → `_` for the crate name).
            if let Some(base) = find_crate_src_dir(crate_name, known_files) {
                return resolve_module_path(&base, rest, known_files)
                    .or_else(|| crate_root_file(&base, known_files));
            }
            let src_dirs = crate_src_dirs(known_files);
            // The root crate's `src/` has no directory name, so it offers no
            // name evidence: in a multi-crate workspace, matching an
            // unmatched crate name against it by module-path coincidence both
            // steals member-crate imports (`fixture_engine::rts_eval` →
            // `src/rts_eval.rs`) and binds external crates (`serde::de` →
            // `src/de.rs`). Only a single-crate repo treats the root crate as
            // a candidate.
            let multi_crate = src_dirs.len() > 1;
            // Unique-crate fallback FIRST: the crate directory does not always
            // match the package name (e.g. package `fixture-engine` in
            // `crates/engine/`). When the module path resolves under exactly
            // one workspace crate, use it; ambiguity or no match stays
            // unresolved. Runs before the dev-target root-crate fallback so a
            // member crate wins over the root crate when both could match.
            let mut matches: Vec<String> = src_dirs
                .into_iter()
                .filter(|base| !(multi_crate && base == "src"))
                .filter_map(|base| resolve_module_path(&base, rest, known_files))
                .collect();
            matches.dedup();
            if matches.len() == 1 {
                return matches.pop();
            }
            // Root-crate fallback, for `use <crate_under_test>::…` from
            // integration tests/benches/examples — tried only when the
            // unique-crate fallback found nothing. Restricted to dev-target
            // files in single-crate repos (P2 review): applying it more
            // broadly would discard the unmatched crate name and bind EXTERNAL
            // crates to local modules (`serde::de` → `src/de.rs`). Requires at
            // least one module segment to resolve so genuinely-external names
            // stay unresolved even from tests.
            if !multi_crate
                && is_dev_target_file(from_file)
                && let Some(root) = crate_root_file("src", known_files)
            {
                let base = parent_dir(&root).to_string();
                return resolve_module_path(&base, rest, known_files);
            }
            None
        }
    }
}

/// Whether a file is a cargo dev target (integration test, bench, example) —
/// the only places `use <crate_under_test>::…` appears, and therefore the
/// only places the root-crate fallback may discard the unmatched crate name.
fn is_dev_target_file(from_file: &str) -> bool {
    from_file.starts_with("tests/")
        || from_file.starts_with("benches/")
        || from_file.starts_with("examples/")
        || from_file.contains("/tests/")
        || from_file.contains("/benches/")
        || from_file.contains("/examples/")
}

/// The crate root source file (`lib.rs` or `main.rs`) directly inside `dir`.
fn crate_root_file(dir: &str, known_files: &HashSet<&str>) -> Option<String> {
    for name in ["lib.rs", "main.rs"] {
        let candidate = format!("{dir}/{name}");
        if known_files.contains(candidate.as_str()) {
            return Some(candidate);
        }
    }
    None
}

/// Find the `src/` dir of the crate enclosing `from_file` by walking up to
/// the nearest ancestor named `src` that contains a `lib.rs` or `main.rs`.
fn current_crate_src_dir(from_file: &str, known_files: &HashSet<&str>) -> Option<String> {
    let mut dir = parent_dir(from_file);
    loop {
        if (dir == "src" || dir.ends_with("/src")) && crate_root_file(dir, known_files).is_some() {
            return Some(dir.to_string());
        }
        if dir.is_empty() || !dir.contains('/') {
            return None;
        }
        dir = parent_dir(dir);
    }
}

/// Find a workspace crate's `src/` dir by crate name. Matches any
/// `…/<dir>/src/lib.rs|main.rs` whose `<dir>` basename equals the crate name
/// with `-`/`_` normalization.
fn find_crate_src_dir(crate_name: &str, known_files: &HashSet<&str>) -> Option<String> {
    let normalized = crate_name.replace('_', "-");
    let mut matches: Vec<&str> = known_files
        .iter()
        .filter_map(|file| {
            let dir = file.rsplit_once('/')?.0; // strip file name
            if !(dir.ends_with("/src") && (file.ends_with("/lib.rs") || file.ends_with("/main.rs")))
            {
                return None;
            }
            let crate_dir = dir.strip_suffix("/src")?;
            let basename = crate_dir
                .rsplit_once('/')
                .map(|(_, b)| b)
                .unwrap_or(crate_dir);
            if basename.replace('_', "-") == normalized {
                Some(dir)
            } else {
                None
            }
        })
        .collect();
    // Deterministic choice when several crates share a directory basename.
    matches.sort_unstable();
    matches.first().map(|s| s.to_string())
}

/// All crate `src/` dirs in the known file set (any `…/src/lib.rs|main.rs`).
fn crate_src_dirs(known_files: &HashSet<&str>) -> Vec<String> {
    let mut dirs: Vec<String> = known_files
        .iter()
        .filter_map(|file| {
            let dir = file.rsplit_once('/')?.0;
            if (file.ends_with("/lib.rs") || file.ends_with("/main.rs"))
                && (dir == "src" || dir.ends_with("/src"))
            {
                Some(dir.to_string())
            } else {
                None
            }
        })
        .collect();
    dirs.sort_unstable();
    dirs.dedup();
    dirs
}

/// Directory of the current module for `self::` paths. For `mod.rs`-style
/// files the module is the file's directory; for `foo.rs` the module `foo`
/// roots at `foo/` next to the file.
fn self_module_dir(from_file: &str) -> String {
    let dir = parent_dir(from_file);
    let file_name = from_file.rsplit('/').next().unwrap_or(from_file);
    if matches!(file_name, "mod.rs" | "lib.rs" | "main.rs") {
        dir.to_string()
    } else if let Some(stem) = file_name.strip_suffix(".rs") {
        format!("{dir}/{stem}")
    } else {
        dir.to_string()
    }
}

/// Directory of the parent module for the first `super::` segment.
fn super_module_dir(from_file: &str) -> String {
    let dir = parent_dir(from_file);
    let file_name = from_file.rsplit('/').next().unwrap_or(from_file);
    if matches!(file_name, "mod.rs" | "lib.rs" | "main.rs") {
        parent_dir(dir).to_string()
    } else {
        dir.to_string()
    }
}

fn parent_dir(path: &str) -> &str {
    match path.rfind('/') {
        Some(idx) => &path[..idx],
        None => "",
    }
}

/// Walk module segments from `base`, returning the deepest module file found.
///
/// Each segment maps to `<dir>/<seg>.rs` or `<dir>/<seg>/mod.rs`; the walk
/// stops at the first segment that matches neither (it names an item inside
/// the last matched module, not a module itself).
fn resolve_module_path(
    base: &str,
    segments: &[&str],
    known_files: &HashSet<&str>,
) -> Option<String> {
    let mut dir = base.to_string();
    let mut resolved: Option<String> = None;
    for seg in segments {
        let file = format!("{dir}/{seg}.rs");
        let mod_file = format!("{dir}/{seg}/mod.rs");
        if known_files.contains(file.as_str()) {
            resolved = Some(file);
        } else if known_files.contains(mod_file.as_str()) {
            resolved = Some(mod_file);
        } else {
            break;
        }
        dir = format!("{dir}/{seg}");
    }
    resolved
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set<'a>(files: &[&'a str]) -> HashSet<&'a str> {
        files.iter().copied().collect()
    }

    #[test]
    fn builtin_crates_never_bind_to_local_modules() {
        // `use std::fs::File;` in a repo that HAS src/fs.rs: the unique-crate
        // fallback must not bind std to the local module (P2 review).
        let known = set(&["src/lib.rs", "src/fs.rs", "src/main.rs"]);
        assert_eq!(resolve_import("src/main.rs", "std::fs::File", &known), None);
        assert_eq!(
            resolve_import("src/main.rs", "std::collections::HashMap", &known),
            None
        );
        assert_eq!(resolve_import("src/main.rs", "core::fmt", &known), None);
        assert_eq!(resolve_import("src/main.rs", "alloc::vec", &known), None);
        // …and in a multi-crate workspace where a member has fs.rs.
        let ws = set(&[
            "src/lib.rs",
            "crates/app/src/main.rs",
            "crates/app/src/fs.rs",
        ]);
        assert_eq!(
            resolve_import("crates/app/src/main.rs", "std::fs", &ws),
            None
        );
    }

    #[test]
    fn resolves_cross_crate_module_import() {
        // `use nestweaver_engine::rts_eval;` from the daemon crate
        let known = set(&[
            "crates/nestweaver-daemon/src/server.rs",
            "crates/nestweaver-daemon/src/main.rs",
            "crates/nestweaver-engine/src/lib.rs",
            "crates/nestweaver-engine/src/rts_eval.rs",
        ]);
        let result = resolve_import(
            "crates/nestweaver-daemon/src/server.rs",
            "nestweaver_engine::rts_eval",
            &known,
        );
        assert_eq!(
            result,
            Some("crates/nestweaver-engine/src/rts_eval.rs".to_string())
        );
    }

    #[test]
    fn resolves_cross_crate_item_import_to_module_file() {
        // `use nestweaver_engine::store::Store;` — `Store` is an item in store.rs
        let known = set(&[
            "crates/nestweaver-daemon/src/server.rs",
            "crates/nestweaver-daemon/src/main.rs",
            "crates/nestweaver-engine/src/lib.rs",
            "crates/nestweaver-engine/src/store.rs",
        ]);
        let result = resolve_import(
            "crates/nestweaver-daemon/src/server.rs",
            "nestweaver_engine::store::Store",
            &known,
        );
        assert_eq!(
            result,
            Some("crates/nestweaver-engine/src/store.rs".to_string())
        );
    }

    #[test]
    fn resolves_crate_relative_import() {
        let known = set(&[
            "crates/nestweaver-daemon/src/main.rs",
            "crates/nestweaver-daemon/src/server.rs",
            "crates/nestweaver-daemon/src/config.rs",
        ]);
        let result = resolve_import(
            "crates/nestweaver-daemon/src/server.rs",
            "crate::config::Settings",
            &known,
        );
        assert_eq!(
            result,
            Some("crates/nestweaver-daemon/src/config.rs".to_string())
        );
    }

    #[test]
    fn resolves_super_import_from_plain_module() {
        let known = set(&["src/lib.rs", "src/foo/bar.rs", "src/foo/helpers.rs"]);
        let result = resolve_import("src/foo/bar.rs", "super::helpers::setup", &known);
        assert_eq!(result, Some("src/foo/helpers.rs".to_string()));
    }

    #[test]
    fn resolves_super_import_from_mod_rs() {
        let known = set(&["src/lib.rs", "src/foo/mod.rs", "src/util.rs"]);
        let result = resolve_import("src/foo/mod.rs", "super::util", &known);
        assert_eq!(result, Some("src/util.rs".to_string()));
    }

    #[test]
    fn resolves_mod_rs_target() {
        let known = set(&[
            "src/lib.rs",
            "src/main.rs",
            "src/engine/mod.rs",
            "src/engine/store.rs",
        ]);
        let result = resolve_import("src/main.rs", "crate::engine::store::Store", &known);
        assert_eq!(result, Some("src/engine/store.rs".to_string()));
    }

    #[test]
    fn resolves_integration_test_crate_under_test() {
        // `use fixture_repo::beta::b;` in tests/beta_it.rs — root crate has no
        // directory name, so the root-crate fallback must resolve it.
        let known = set(&["src/lib.rs", "src/beta.rs", "tests/beta_it.rs"]);
        let result = resolve_import("tests/beta_it.rs", "fixture_repo::beta::b", &known);
        assert_eq!(result, Some("src/beta.rs".to_string()));
    }

    #[test]
    fn external_crate_stays_unresolved() {
        // `use serde::de::Deserialize;` must NOT fall back to the root crate:
        // no module segment exists under src/.
        let known = set(&["src/lib.rs", "src/beta.rs", "tests/it.rs"]);
        let result = resolve_import("tests/it.rs", "serde::de::Deserialize", &known);
        assert_eq!(result, None);
    }

    #[test]
    fn std_import_stays_unresolved() {
        let known = set(&["src/lib.rs", "src/main.rs"]);
        let result = resolve_import("src/main.rs", "std::collections::HashMap", &known);
        assert_eq!(result, None);
    }

    #[test]
    fn named_crate_falls_back_to_crate_root() {
        // `use nestweaver_engine::Engine;` — `Engine` is defined in lib.rs
        let known = set(&[
            "crates/nestweaver-daemon/src/main.rs",
            "crates/nestweaver-engine/src/lib.rs",
            "crates/nestweaver-engine/src/store.rs",
        ]);
        let result = resolve_import(
            "crates/nestweaver-daemon/src/main.rs",
            "nestweaver_engine::Engine",
            &known,
        );
        assert_eq!(
            result,
            Some("crates/nestweaver-engine/src/lib.rs".to_string())
        );
    }

    #[test]
    fn dash_underscore_crate_name_equivalence() {
        let known = set(&[
            "crates/foo-bar/src/lib.rs",
            "crates/foo-bar/src/thing.rs",
            "src/main.rs",
        ]);
        let result = resolve_import("src/main.rs", "foo_bar::thing", &known);
        assert_eq!(result, Some("crates/foo-bar/src/thing.rs".to_string()));
    }

    #[test]
    fn unique_crate_fallback_when_dir_differs_from_package_name() {
        // Package `fixture-engine` lives in crates/engine/ — the directory
        // basename does not match, but the module path resolves under exactly
        // one crate.
        let known = set(&[
            "crates/engine/src/lib.rs",
            "crates/engine/src/rts_eval.rs",
            "crates/daemon/src/main.rs",
            "crates/daemon/src/server.rs",
        ]);
        let result = resolve_import(
            "crates/daemon/src/server.rs",
            "fixture_engine::rts_eval",
            &known,
        );
        assert_eq!(result, Some("crates/engine/src/rts_eval.rs".to_string()));
    }

    #[test]
    fn ambiguous_module_path_stays_unresolved() {
        // Two crates both have src/util.rs — the unique-crate fallback must
        // not guess.
        let known = set(&[
            "crates/a/src/lib.rs",
            "crates/a/src/util.rs",
            "crates/b/src/lib.rs",
            "crates/b/src/util.rs",
            "crates/c/src/main.rs",
        ]);
        let result = resolve_import("crates/c/src/main.rs", "other_crate::util", &known);
        assert_eq!(result, None);
    }

    #[test]
    fn dev_target_prefers_member_crate_over_root_crate() {
        // Workspace with a root crate AND a member whose directory differs
        // from its package name; both happen to have `rts_eval.rs`.
        // `use fixture_engine::rts_eval` from tests/ must resolve to the
        // member crate — the unique-crate fallback runs before the
        // dev-target root-crate fallback.
        let known = set(&[
            "src/lib.rs",
            "src/rts_eval.rs",
            "crates/engine/src/lib.rs",
            "crates/engine/src/rts_eval.rs",
            "tests/it.rs",
        ]);
        let result = resolve_import("tests/it.rs", "fixture_engine::rts_eval", &known);
        assert_eq!(result, Some("crates/engine/src/rts_eval.rs".to_string()));
    }

    #[test]
    fn external_crate_from_test_does_not_bind_root_module_in_workspace() {
        // In a multi-crate workspace the root crate offers no name evidence,
        // so `use serde::de` from a test file must NOT bind to the root
        // crate's src/de.rs.
        let known = set(&[
            "src/lib.rs",
            "src/de.rs",
            "crates/engine/src/lib.rs",
            "tests/it.rs",
        ]);
        let result = resolve_import("tests/it.rs", "serde::de", &known);
        assert_eq!(result, None);
    }
}
