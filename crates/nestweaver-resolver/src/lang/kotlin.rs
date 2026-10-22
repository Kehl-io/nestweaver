use std::collections::{HashMap, HashSet};

use nestweaver_parser::{RawReference, RawSymbol, ReferenceKind};
use nestweaver_schema::{Language, SymbolKind};

use crate::util::parent_dir;

pub fn resolve_import(
    from_file: &str,
    specifier: &str,
    known_files: &HashSet<&str>,
) -> Option<String> {
    let _ = from_file;

    if specifier.ends_with(".*") {
        return None;
    }

    let mut best: Option<&str> = None;
    for ext in &[".kt", ".java"] {
        let candidate = format!("{}{ext}", specifier.replace('.', "/"));
        for &file in known_files {
            if (file == candidate || file.ends_with(&format!("/{candidate}")))
                && (best.is_none() || file.len() < best.unwrap().len())
            {
                best = Some(file);
            }
        }
    }
    if let Some(f) = best {
        return Some(f.to_string());
    }

    None
}

/// Kotlin and Java files by package, with their top-level declarations.
///
/// A Kotlin import names a declaration of a PACKAGE, not a file: `import
/// com.acme.a.helper` is satisfied by any file declaring `package com.acme.a`
/// with a top-level `helper`, whatever the file is called. Java's one public
/// class per file usually puts `Foo` in `Foo.java`, but a package-private
/// top-level class can live in any file of its package, and a nested class
/// `Outer.Inner` lives in `Outer.java`.
pub struct JvmPackageIndex<'a> {
    /// Declared package → files declaring it.
    declared: HashMap<&'a str, Vec<&'a str>>,
    /// Files whose package clause is not known (an unchanged file in an
    /// incremental pass carries no references), as `(directory, file)` keyed
    /// by the directory's last segment. Their package is inferred from the
    /// conventional directory layout; the key keeps that lookup from scanning
    /// every such file per import.
    undeclared: HashMap<&'a str, Vec<(&'a str, &'a str)>>,
    /// File → its top-level declaration names.
    top_level: HashMap<&'a str, HashSet<&'a str>>,
}

impl<'a> JvmPackageIndex<'a> {
    pub fn new(
        files: &'a [(String, Vec<RawSymbol>, Vec<RawReference>)],
        language_of: &dyn Fn(&str) -> Language,
    ) -> Self {
        let mut declared: HashMap<&'a str, Vec<&'a str>> = HashMap::new();
        let mut undeclared: HashMap<&'a str, Vec<(&'a str, &'a str)>> = HashMap::new();
        let mut top_level = HashMap::new();
        for (file, symbols, references) in files {
            if !matches!(language_of(file), Language::Kotlin | Language::Java) {
                continue;
            }
            let mut packages = references
                .iter()
                .filter(|reference| reference.kind == ReferenceKind::PackageBinding);
            match (packages.next(), packages.next()) {
                (Some(package), None) => declared
                    .entry(package.name.as_str())
                    .or_default()
                    .push(file.as_str()),
                _ => {
                    let dir = parent_dir(file);
                    let last = dir.rsplit('/').next().unwrap_or(dir);
                    undeclared
                        .entry(last)
                        .or_default()
                        .push((dir, file.as_str()));
                }
            }
            top_level.insert(
                file.as_str(),
                symbols
                    .iter()
                    .filter(|symbol| {
                        symbol.parent_name.is_none() && symbol.kind != SymbolKind::Method
                    })
                    .map(|symbol| symbol.name.as_str())
                    .collect(),
            );
        }
        for files in declared.values_mut() {
            files.sort_unstable();
        }
        Self {
            declared,
            undeclared,
            top_level,
        }
    }

    /// Files of `package`: those declaring it, and undeclared files whose
    /// directory spells it (`com/acme/a` for `com.acme.a`).
    fn package_files(&self, package: &str) -> Vec<&'a str> {
        let mut files = self.declared.get(package).cloned().unwrap_or_default();
        let last = package.rsplit('.').next().unwrap_or(package);
        if let Some(candidates) = self.undeclared.get(last) {
            let dir = package.replace('.', "/");
            let suffix = format!("/{dir}");
            files.extend(
                candidates
                    .iter()
                    .filter(|(parent, _)| *parent == dir || parent.ends_with(&suffix))
                    .map(|(_, file)| *file),
            );
        }
        files.sort_unstable();
        files
    }

    /// Every file an import specifier names, sorted; empty when none.
    ///
    /// `a.b.C` is the top-level `C` of package `a.b` (tried longest package
    /// first, so `a.b.Outer.Inner` reaches `Outer` in `a.b`). A specifier
    /// that names no declaration but is itself a package is a wildcard
    /// import (`import a.b.*` arrives as `a.b`), naming every file of it.
    pub fn resolve(&self, specifier: &str) -> Vec<&'a str> {
        let specifier = specifier.trim_end_matches(".*");
        let segments: Vec<&str> = specifier.split('.').collect();
        for split in (1..segments.len()).rev() {
            let package = segments[..split].join(".");
            let name = segments[split];
            let files: Vec<&'a str> = self
                .package_files(&package)
                .into_iter()
                .filter(|file| {
                    self.top_level
                        .get(file)
                        .is_some_and(|names| names.contains(name))
                })
                .collect();
            if !files.is_empty() {
                return files;
            }
        }
        self.package_files(specifier)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set<'a>(files: &[&'a str]) -> HashSet<&'a str> {
        files.iter().copied().collect()
    }

    #[test]
    fn resolves_kotlin_package_import() {
        let known = set(&["src/main/kotlin/com/example/utils/Helper.kt"]);
        let result = resolve_import(
            "src/main/kotlin/com/example/Main.kt",
            "com.example.utils.Helper",
            &known,
        );
        assert_eq!(
            result,
            Some("src/main/kotlin/com/example/utils/Helper.kt".to_string())
        );
    }

    #[test]
    fn wildcard_import_returns_none() {
        let known = set(&["src/com/example/Foo.kt"]);
        let result = resolve_import("src/Main.kt", "com.example.*", &known);
        assert_eq!(result, None);
    }
}
