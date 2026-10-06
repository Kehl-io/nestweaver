use std::collections::{HashMap, HashSet};

use nestweaver_parser::{RawReference, RawSymbol, ReferenceKind};
use nestweaver_schema::{Language, Visibility};

use crate::lang;
use crate::workspace::WorkspaceContext;

/// Tracks an aliased import binding (e.g., `use a::b as c;`).
///
/// Populated from [`ReferenceKind::ImportAlias`] references emitted by the
/// parser for Rust paths and exact JS/TS/Python imported bindings.
#[derive(Debug, Clone)]
pub struct NamedBinding {
    pub scope: Option<nestweaver_parser::LexicalScope>,
    /// The local alias used in the importing file.
    pub local_name: String,
    /// The original exported name from the source file.
    pub original_name: String,
    /// The file that exports the original name, or a failed JS/TS import.
    pub source_file: Option<String>,
    /// Import location, used to keep function-local bindings inside their owner.
    pub start_line: u32,
}

/// Tracks what each file exports and what it imports.
pub struct ImportGraph {
    /// file → [(specifier, resolved_file)]
    resolved_imports: HashMap<String, Vec<(String, String)>>,
    /// file → [exported_symbol_names]
    exports: HashMap<String, Vec<String>>,
    /// file → [named bindings (aliased imports)]
    named_bindings: HashMap<String, Vec<NamedBinding>>,
    export_aliases: HashMap<String, HashMap<String, (String, Option<String>)>>,
    star_exports: HashMap<String, Vec<String>>,
    go_packages: HashMap<String, String>,
    unresolved_exports: HashMap<String, HashSet<String>>,
    incomplete_stars: HashSet<String>,
}

impl ImportGraph {
    /// Returns true if `from` file imports `specifier` which resolves to `to`.
    pub fn resolves(&self, from: &str, specifier: &str, to: &str) -> bool {
        if let Some(imports) = self.resolved_imports.get(from) {
            imports
                .iter()
                .any(|(spec, resolved)| spec == specifier && resolved == to)
        } else {
            false
        }
    }

    /// Returns the names exported by the given file.
    pub fn exports_of(&self, file: &str) -> Vec<String> {
        self.exports.get(file).cloned().unwrap_or_default()
    }

    /// Returns the (specifier, resolved_file) pairs imported by the given file.
    pub fn imports_of(&self, file: &str) -> Vec<(String, String)> {
        self.resolved_imports.get(file).cloned().unwrap_or_default()
    }

    /// Returns the named bindings (aliased imports) for the given file.
    pub fn bindings_of(&self, file: &str) -> &[NamedBinding] {
        self.named_bindings
            .get(file)
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }

    /// Follow only exact named public exports, bounded like the existing barrel walk.
    /// A public declaration name is not automatically another public export key.
    pub fn exported_target<'a>(
        &'a self,
        file: &'a str,
        name: &'a str,
    ) -> Option<(&'a str, &'a str, usize)> {
        let mut targets = HashMap::new();
        let complete =
            self.collect_export_targets(file, name, 0, &mut HashSet::new(), &mut targets, &mut 256);
        if complete && targets.len() == 1 {
            targets
                .into_iter()
                .next()
                .map(|((file, name), depth)| (file, name, depth))
        } else {
            None
        }
    }

    fn collect_export_targets<'a>(
        &'a self,
        file: &'a str,
        name: &'a str,
        depth: usize,
        visited: &mut HashSet<(&'a str, &'a str)>,
        targets: &mut HashMap<(&'a str, &'a str), usize>,
        remaining: &mut usize,
    ) -> bool {
        if depth > 3 || *remaining == 0 || targets.len() > 1 {
            return false;
        }
        *remaining -= 1;
        if !visited.insert((file, name)) {
            return true;
        }
        let mut complete = true;
        if self
            .unresolved_exports
            .get(file)
            .is_some_and(|names| names.contains(name))
        {
            visited.remove(&(file, name));
            return false;
        }
        if let Some((local, source)) = self
            .export_aliases
            .get(file)
            .and_then(|aliases| aliases.get(name))
        {
            if let Some(source) = source {
                complete = self.collect_export_targets(
                    source,
                    local,
                    depth + 1,
                    visited,
                    targets,
                    remaining,
                );
            } else {
                targets
                    .entry((file, local))
                    .and_modify(|old| *old = (*old).min(depth))
                    .or_insert(depth);
            }
        } else if name != "default" {
            if self.incomplete_stars.contains(file) {
                visited.remove(&(file, name));
                return false;
            }
            for source in self.star_exports.get(file).into_iter().flatten() {
                if !self.collect_export_targets(
                    source,
                    name,
                    depth + 1,
                    visited,
                    targets,
                    remaining,
                ) {
                    complete = false;
                    break;
                }
                if targets.len() > 1 {
                    complete = false;
                    break;
                }
            }
        }
        visited.remove(&(file, name));
        complete
    }

    pub(crate) fn same_go_package(&self, from: &str, to: &str) -> bool {
        self.go_packages
            .get(from)
            .is_some_and(|package| self.go_packages.get(to) == Some(package))
    }

    /// Only a resolved `super` import grants access to its parent module's private items.
    pub(crate) fn resolves_parent(&self, from: &str, to: &str) -> bool {
        fn module_path(file: &str) -> &str {
            for suffix in ["/mod.rs", "/lib.rs", "/main.rs"] {
                if let Some(module) = file.strip_suffix(suffix) {
                    return module;
                }
            }
            file.strip_suffix(".rs").unwrap_or(file)
        }
        let parent = module_path(to);
        let child = module_path(from);
        child
            .strip_prefix(parent)
            .is_some_and(|rest| rest.starts_with('/'))
            && self.resolved_imports.get(from).is_some_and(|imports| {
                imports.iter().any(|(specifier, target)| {
                    target == to && (specifier == "super" || specifier.starts_with("super::"))
                })
            })
    }

    /// Returns all resolved imports across all files as (source_file, specifier, target_file) triples.
    pub fn all_resolved_imports(&self) -> Vec<(&str, &str, &str)> {
        self.resolved_imports
            .iter()
            .flat_map(|(src, imports)| {
                imports
                    .iter()
                    .map(move |(spec, tgt)| (src.as_str(), spec.as_str(), tgt.as_str()))
            })
            .collect()
    }
}

/// Build an import graph from parsed file data.
///
/// v1: all top-level symbols are considered exported.
pub fn build_import_graph(
    files: &[(String, Vec<RawSymbol>, Vec<RawReference>)],
    language: Language,
    workspace_ctx: &WorkspaceContext,
) -> ImportGraph {
    build_import_graph_with_languages(files, language, None, workspace_ctx)
}

pub(crate) fn build_import_graph_with_languages(
    files: &[(String, Vec<RawSymbol>, Vec<RawReference>)],
    fallback_language: Language,
    file_languages: Option<&HashMap<String, Language>>,
    workspace_ctx: &WorkspaceContext,
) -> ImportGraph {
    let known_files: HashSet<&str> = files.iter().map(|(path, _, _)| path.as_str()).collect();

    let mut exports: HashMap<String, Vec<String>> = HashMap::new();
    let mut resolved_imports: HashMap<String, Vec<(String, String)>> = HashMap::new();
    let mut named_bindings: HashMap<String, Vec<NamedBinding>> = HashMap::new();
    let mut export_aliases = HashMap::new();
    let mut star_exports = HashMap::new();
    let mut go_packages = HashMap::new();
    let mut unresolved_exports: HashMap<String, HashSet<String>> = HashMap::new();
    let mut incomplete_stars = HashSet::new();

    for (file_path, symbols, references) in files {
        let language = file_languages
            .and_then(|languages| languages.get(file_path))
            .copied()
            .unwrap_or(fallback_language);
        if language == Language::Go {
            let packages: Vec<_> = references
                .iter()
                .filter(|reference| reference.kind == ReferenceKind::PackageBinding)
                .collect();
            if packages.len() == 1 {
                go_packages.insert(file_path.clone(), packages[0].name.clone());
            }
        }
        // v2: filter by visibility — only non-private symbols are exported
        let exported_names: Vec<String> = symbols
            .iter()
            .filter(|s| !matches!(s.visibility, Visibility::Private))
            .map(|s| s.name.clone())
            .collect();
        exports.insert(file_path.clone(), exported_names);
        for reference in references
            .iter()
            .filter(|reference| reference.kind == ReferenceKind::ExportAlias)
        {
            let specifier = reference.receiver.as_deref().or_else(|| {
                references
                    .iter()
                    .find(|binding| {
                        binding.kind == ReferenceKind::ImportAlias
                            && binding.name == reference.context
                            && !symbols.iter().any(|symbol| {
                                matches!(
                                    symbol.kind,
                                    nestweaver_schema::SymbolKind::Function
                                        | nestweaver_schema::SymbolKind::Method
                                        | nestweaver_schema::SymbolKind::Class
                                ) && symbol.start_line <= binding.start_line
                                    && binding.start_line <= symbol.end_line
                            })
                    })
                    .map(|binding| binding.context.as_str())
            });
            if specifier.is_some_and(|specifier| {
                resolve_specifier(file_path, specifier, &known_files, language, workspace_ctx)
                    .is_none()
            }) {
                if reference.name == "*" {
                    incomplete_stars.insert(file_path.clone());
                } else {
                    unresolved_exports
                        .entry(file_path.clone())
                        .or_default()
                        .insert(reference.name.clone());
                }
            }
        }
        star_exports.insert(
            file_path.clone(),
            references
                .iter()
                .filter(|reference| {
                    reference.kind == ReferenceKind::ExportAlias && reference.name == "*"
                })
                .filter_map(|reference| {
                    resolve_specifier(
                        file_path,
                        reference.receiver.as_deref()?,
                        &known_files,
                        language,
                        workspace_ctx,
                    )
                })
                .collect(),
        );
        export_aliases.insert(
            file_path.clone(),
            references
                .iter()
                .filter(|reference| {
                    reference.kind == ReferenceKind::ExportAlias && reference.name != "*"
                })
                .filter_map(|reference| {
                    // A module-local exported name can itself be a precise
                    // imported binding. Function-local imports cannot forward
                    // an unrelated declaration exported at module scope.
                    let imported = references.iter().find(|binding| {
                        binding.kind == ReferenceKind::ImportAlias
                            && binding.name == reference.context
                            && !symbols.iter().any(|symbol| {
                                matches!(
                                    symbol.kind,
                                    nestweaver_schema::SymbolKind::Function
                                        | nestweaver_schema::SymbolKind::Method
                                        | nestweaver_schema::SymbolKind::Class
                                ) && symbol.start_line <= binding.start_line
                                    && binding.start_line <= symbol.end_line
                            })
                    });
                    let (local, source) = match reference.receiver.as_deref() {
                        Some(specifier) => (
                            reference.context.clone(),
                            Some(resolve_specifier(
                                file_path,
                                specifier,
                                &known_files,
                                language,
                                workspace_ctx,
                            )?),
                        ),
                        None if imported.is_some() => {
                            let imported = imported.expect("matched imported export");
                            (
                                imported.receiver.clone()?,
                                Some(resolve_specifier(
                                    file_path,
                                    &imported.context,
                                    &known_files,
                                    language,
                                    workspace_ctx,
                                )?),
                            )
                        }
                        None => (reference.context.clone(), None),
                    };
                    Some((reference.name.clone(), (local, source)))
                })
                .collect(),
        );

        // Resolve import references
        let mut imports: Vec<(String, String)> = Vec::new();
        let mut bindings: Vec<NamedBinding> = Vec::new();
        for reference in references {
            // Aliased import (`use a::b as c;`): `name` is the local alias,
            // `context` is the full original path. Bind the alias to the
            // last path segment of the resolved source file. The path itself
            // is already covered by its own Import reference, so it is not
            // added to `imports` again here.
            if reference.kind == ReferenceKind::ImportAlias {
                let resolved = resolve_specifier(
                    file_path,
                    &reference.context,
                    &known_files,
                    language,
                    workspace_ctx,
                );
                if resolved.is_none()
                    && !matches!(language, Language::JavaScript | Language::TypeScript)
                {
                    continue;
                }
                let original_name = reference
                    .receiver
                    .as_deref()
                    .unwrap_or(&reference.context)
                    .rsplit("::")
                    .next()
                    .unwrap_or(reference.context.as_str())
                    .to_string();
                bindings.push(NamedBinding {
                    scope: reference.scope,
                    local_name: reference.name.clone(),
                    original_name,
                    source_file: resolved,
                    start_line: reference.start_line,
                });
                continue;
            }
            if !matches!(
                reference.kind,
                ReferenceKind::Import | ReferenceKind::Includes | ReferenceKind::Uses
            ) {
                continue;
            }
            let specifier = &reference.name;
            if let Some(resolved) =
                resolve_specifier(file_path, specifier, &known_files, language, workspace_ctx)
            {
                imports.push((specifier.clone(), resolved));
            }
        }
        resolved_imports.insert(file_path.clone(), imports);
        if !bindings.is_empty() {
            named_bindings.insert(file_path.clone(), bindings);
        }
    }

    ImportGraph {
        resolved_imports,
        exports,
        named_bindings,
        export_aliases,
        star_exports,
        go_packages,
        unresolved_exports,
        incomplete_stars,
    }
}

fn resolve_specifier(
    from_file: &str,
    specifier: &str,
    known_files: &HashSet<&str>,
    language: Language,
    workspace_ctx: &WorkspaceContext,
) -> Option<String> {
    match language {
        Language::JavaScript | Language::TypeScript => {
            lang::javascript::resolve_import(from_file, specifier, known_files, workspace_ctx)
        }
        Language::Java => lang::java::resolve_import(from_file, specifier, known_files),
        Language::Go => lang::go_lang::resolve_import(from_file, specifier, known_files),
        Language::Python => lang::python::resolve_import(from_file, specifier, known_files),
        // nw-349 (cross-lane) / nw-351: C++ `#include` is captured by
        // `queries/cpp.scm` and was then thrown away here, while the identical
        // C directive resolved through `lang::c`. An import edge is one of only
        // two routes a cross-file reference has (the other is a same-directory
        // match), so C++ had no cross-directory resolution at all. The C
        // resolver already handles the exact syntax, including the `<`/`>` and
        // quote stripping C++ needs, so this is one arm, not a new module.
        Language::Cpp => lang::c::resolve_import(from_file, specifier, known_files),
        Language::Rust => lang::rust::resolve_import(from_file, specifier, known_files),
        Language::C => lang::c::resolve_import(from_file, specifier, known_files),
        Language::CSharp => lang::csharp::resolve_import(from_file, specifier, known_files),
        Language::Kotlin => lang::kotlin::resolve_import(from_file, specifier, known_files),
        Language::Php => lang::php::resolve_import(from_file, specifier, known_files),
        Language::Ruby => lang::ruby::resolve_import(from_file, specifier, known_files),
        Language::Dart => lang::dart::resolve_import(from_file, specifier, known_files),
        Language::Swift => lang::swift::resolve_import(from_file, specifier, known_files),
        Language::Scala => lang::scala::resolve_import(from_file, specifier, known_files),
        Language::Groovy => lang::groovy::resolve_import(from_file, specifier, known_files),
        Language::Fortran => lang::fortran::resolve_import(from_file, specifier, known_files),
        Language::Pascal => lang::pascal::resolve_import(from_file, specifier, known_files),
        Language::SystemVerilog => {
            lang::systemverilog::resolve_import(from_file, specifier, known_files)
        }
        Language::Zig => lang::zig::resolve_import(from_file, specifier, known_files),
        Language::ObjectiveC => lang::objc::resolve_import(from_file, specifier, known_files),
        Language::Lua => lang::lua::resolve_import(from_file, specifier, known_files),
        Language::PowerShell => lang::powershell::resolve_import(from_file, specifier, known_files),
        Language::Julia => lang::julia::resolve_import(from_file, specifier, known_files),
        Language::Cobol | Language::Bash | Language::Elixir | Language::Sql | Language::Hcl => None,
        Language::Vue | Language::Svelte | Language::Astro => {
            lang::javascript::resolve_import(from_file, specifier, known_files, workspace_ctx)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nestweaver_parser::ReferenceKind;
    use nestweaver_schema::{SymbolKind, Visibility};

    fn make_symbol(name: &str) -> RawSymbol {
        RawSymbol {
            name: name.to_string(),
            kind: SymbolKind::Function,
            start_line: 1,
            end_line: 1,
            signature: format!("function {name}()"),
            content_hash: String::new(),
            is_entry_point: false,
            entry_point_kind: None,
            visibility: Visibility::Inferred,
            type_info: None,
            parent_name: None,
            scope_chain: None,
        }
    }

    fn make_import_ref(specifier: &str) -> RawReference {
        RawReference {
            scope: None,
            name: specifier.to_string(),
            kind: ReferenceKind::Import,
            start_line: 1,
            context: String::new(),
            receiver: None,
        }
    }

    fn make_include_ref(specifier: &str) -> RawReference {
        RawReference {
            scope: None,
            name: specifier.to_string(),
            kind: ReferenceKind::Includes,
            start_line: 1,
            context: String::new(),
            receiver: None,
        }
    }

    /// nw-349 (cross-lane) / nw-351. A C++ `#include` is captured by
    /// `queries/cpp.scm` and was then discarded, because `resolve_specifier`
    /// had `Language::Cpp => None` while its neighbour `Language::C` used a
    /// resolver that already strips `"`, `<` and `>`. The consequence is not
    /// one missing edge family: an import edge is one of only two routes a
    /// cross-file reference has, so C++ had no cross-directory resolution at
    /// all. This is INDEPENDENT of nw-352 — extracting header classes creates
    /// no import edge.
    #[test]
    fn cpp_include_of_a_corpus_file_resolves_to_that_file() {
        let files = vec![
            (
                "src/app/main.cpp".to_string(),
                vec![make_symbol("main")],
                vec![
                    make_include_ref("sensor.h"),
                    make_include_ref("common/types.h"),
                ],
            ),
            (
                "src/app/sensor.h".to_string(),
                vec![make_symbol("Sensor")],
                vec![],
            ),
            (
                "src/common/types.h".to_string(),
                vec![make_symbol("Reading")],
                vec![],
            ),
        ];

        let graph = build_import_graph(&files, Language::Cpp, &WorkspaceContext::default());
        assert!(
            graph.resolves("src/app/main.cpp", "sensor.h", "src/app/sensor.h"),
            "a same-directory #include must resolve: {:?}",
            graph.imports_of("src/app/main.cpp")
        );
        assert!(
            graph.resolves("src/app/main.cpp", "common/types.h", "src/common/types.h"),
            "a cross-directory #include must resolve: {:?}",
            graph.imports_of("src/app/main.cpp")
        );
    }

    /// The counterweight to the arm above: a system include names no file in
    /// the corpus and must resolve to NOTHING, or the fix mints an edge to
    /// whatever happened to share the name.
    #[test]
    fn cpp_system_include_resolves_to_nothing() {
        let files = vec![
            (
                "src/app/main.cpp".to_string(),
                vec![make_symbol("main")],
                vec![make_include_ref("vector")],
            ),
            (
                "src/app/sensor.h".to_string(),
                vec![make_symbol("Sensor")],
                vec![],
            ),
        ];

        let graph = build_import_graph(&files, Language::Cpp, &WorkspaceContext::default());
        assert!(
            graph.imports_of("src/app/main.cpp").is_empty(),
            "a system include names nothing in the corpus: {:?}",
            graph.imports_of("src/app/main.cpp")
        );
    }

    #[test]
    fn resolves_relative_import_js() {
        let files = vec![
            (
                "src/main.js".to_string(),
                vec![make_symbol("main")],
                vec![make_import_ref("./helper")],
            ),
            (
                "src/helper.js".to_string(),
                vec![make_symbol("helper")],
                vec![],
            ),
        ];

        let graph = build_import_graph(&files, Language::JavaScript, &WorkspaceContext::default());
        assert!(
            graph.resolves("src/main.js", "./helper", "src/helper.js"),
            "should resolve ./helper to src/helper.js"
        );
    }

    #[test]
    fn resolves_barrel_import_js() {
        let files = vec![
            (
                "src/main.js".to_string(),
                vec![make_symbol("main")],
                vec![make_import_ref("./utils")],
            ),
            (
                "src/utils/index.js".to_string(),
                vec![make_symbol("utilA"), make_symbol("utilB")],
                vec![],
            ),
        ];

        let graph = build_import_graph(&files, Language::JavaScript, &WorkspaceContext::default());
        assert!(
            graph.resolves("src/main.js", "./utils", "src/utils/index.js"),
            "should resolve ./utils to src/utils/index.js"
        );
    }

    #[test]
    fn tracks_exported_names() {
        let files = vec![(
            "src/api.js".to_string(),
            vec![make_symbol("fetchUser"), make_symbol("createUser")],
            vec![],
        )];

        let graph = build_import_graph(&files, Language::JavaScript, &WorkspaceContext::default());
        let exports = graph.exports_of("src/api.js");
        assert!(exports.contains(&"fetchUser".to_string()));
        assert!(exports.contains(&"createUser".to_string()));
    }

    #[test]
    fn named_binding_accessor_works() {
        let mut bindings = HashMap::new();
        bindings.insert(
            "src/main.js".to_string(),
            vec![NamedBinding {
                scope: None,
                local_name: "MyAlias".to_string(),
                original_name: "OriginalName".to_string(),
                source_file: Some("src/lib.js".to_string()),
                start_line: 1,
            }],
        );
        let graph = ImportGraph {
            resolved_imports: HashMap::new(),
            exports: HashMap::new(),
            named_bindings: bindings,
            export_aliases: HashMap::new(),
            star_exports: HashMap::new(),
            go_packages: HashMap::new(),
            unresolved_exports: HashMap::new(),
            incomplete_stars: HashSet::new(),
        };
        let result = graph.bindings_of("src/main.js");
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].local_name, "MyAlias");
        assert_eq!(result[0].original_name, "OriginalName");
    }

    #[test]
    fn populates_named_bindings_from_import_alias_refs() {
        let make_alias_ref = |alias: &str, specifier: &str| RawReference {
            scope: None,
            name: alias.to_string(),
            kind: ReferenceKind::ImportAlias,
            start_line: 1,
            context: specifier.to_string(),
            receiver: None,
        };
        let files = vec![
            (
                "src/main.rs".to_string(),
                vec![make_symbol("main")],
                vec![
                    make_import_ref("crate::config::load"),
                    make_alias_ref("load_config", "crate::config::load"),
                    // External crate: stays unresolved, so no binding.
                    make_alias_ref("de", "serde::de"),
                ],
            ),
            (
                "src/config.rs".to_string(),
                vec![make_symbol("load")],
                vec![],
            ),
        ];

        let graph = build_import_graph(&files, Language::Rust, &WorkspaceContext::default());
        let bindings = graph.bindings_of("src/main.rs");
        assert_eq!(bindings.len(), 1, "only the resolved alias gets a binding");
        assert_eq!(bindings[0].local_name, "load_config");
        assert_eq!(bindings[0].original_name, "load");
        assert_eq!(bindings[0].source_file.as_deref(), Some("src/config.rs"));
        // The ImportAlias reference must not duplicate the resolved import.
        assert_eq!(
            graph.imports_of("src/main.rs"),
            vec![(
                "crate::config::load".to_string(),
                "src/config.rs".to_string()
            )]
        );
    }

    #[test]
    fn imports_of_returns_resolved_pairs() {
        let files = vec![
            (
                "src/main.js".to_string(),
                vec![],
                vec![make_import_ref("./helper"), make_import_ref("lodash")],
            ),
            (
                "src/helper.js".to_string(),
                vec![make_symbol("helper")],
                vec![],
            ),
        ];

        let graph = build_import_graph(&files, Language::JavaScript, &WorkspaceContext::default());
        let imports = graph.imports_of("src/main.js");
        // lodash is unresolved (package import), helper.js should be resolved
        assert!(
            imports.iter().any(|(spec, _)| spec == "./helper"),
            "should have ./helper import"
        );
        // lodash should not appear in resolved imports
        assert!(
            !imports.iter().any(|(spec, _)| spec == "lodash"),
            "lodash should not be resolved"
        );
    }

    #[test]
    fn review_star_walk_budget_fails_closed_after_a_known_target() {
        let mut graph = ImportGraph {
            resolved_imports: HashMap::new(),
            exports: HashMap::new(),
            named_bindings: HashMap::new(),
            go_packages: HashMap::new(),
            unresolved_exports: HashMap::new(),
            incomplete_stars: HashSet::new(),
            export_aliases: HashMap::from([(
                "leaf.js".into(),
                HashMap::from([("helper".into(), ("helper".into(), None))]),
            )]),
            star_exports: HashMap::new(),
        };
        let mut branches = vec!["leaf.js".into()];
        branches.extend((0..300).map(|index| format!("branch{index}.js")));
        graph.star_exports.insert("index.js".into(), branches);
        assert_eq!(
            graph.exported_target("index.js", "helper"),
            None,
            "one discovered export is insufficient when uniqueness search exceeds its budget"
        );
    }

    #[test]
    fn review_star_walk_duplicate_paths_keep_one_target() {
        let graph = ImportGraph {
            resolved_imports: HashMap::new(),
            exports: HashMap::new(),
            named_bindings: HashMap::new(),
            go_packages: HashMap::new(),
            unresolved_exports: HashMap::new(),
            incomplete_stars: HashSet::new(),
            export_aliases: HashMap::from([(
                "leaf.js".into(),
                HashMap::from([("helper".into(), ("helper".into(), None))]),
            )]),
            star_exports: HashMap::from([
                ("index.js".into(), vec!["a.js".into(), "b.js".into()]),
                ("a.js".into(), vec!["leaf.js".into()]),
                ("b.js".into(), vec!["leaf.js".into()]),
            ]),
        };
        assert_eq!(
            graph.exported_target("index.js", "helper"),
            Some(("leaf.js", "helper", 2))
        );
    }
}
