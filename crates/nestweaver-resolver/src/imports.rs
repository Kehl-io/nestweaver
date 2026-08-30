use std::collections::{HashMap, HashSet};

use nestweaver_parser::{RawReference, RawSymbol, ReferenceKind};
use nestweaver_schema::{Language, Visibility};

use crate::lang;
use crate::workspace::WorkspaceContext;

/// Tracks an aliased import binding (e.g., `use a::b as c;`).
///
/// Populated from [`ReferenceKind::ImportAlias`] references emitted by the
/// parser (currently only Rust `use ... as ...` clauses produce them).
#[derive(Debug, Clone)]
pub struct NamedBinding {
    /// The local alias used in the importing file.
    pub local_name: String,
    /// The original exported name from the source file.
    pub original_name: String,
    /// The file that exports the original name.
    pub source_file: String,
}

/// Tracks what each file exports and what it imports.
pub struct ImportGraph {
    /// file → [(specifier, resolved_file)]
    resolved_imports: HashMap<String, Vec<(String, String)>>,
    /// file → [exported_symbol_names]
    exports: HashMap<String, Vec<String>>,
    /// file → [named bindings (aliased imports)]
    named_bindings: HashMap<String, Vec<NamedBinding>>,
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
    pub fn bindings_of(&self, file: &str) -> Vec<&NamedBinding> {
        self.named_bindings
            .get(file)
            .map(|v| v.iter().collect())
            .unwrap_or_default()
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
    let known_files: HashSet<&str> = files.iter().map(|(path, _, _)| path.as_str()).collect();

    let mut exports: HashMap<String, Vec<String>> = HashMap::new();
    let mut resolved_imports: HashMap<String, Vec<(String, String)>> = HashMap::new();
    let mut named_bindings: HashMap<String, Vec<NamedBinding>> = HashMap::new();

    for (file_path, symbols, references) in files {
        // v2: filter by visibility — only non-private symbols are exported
        let exported_names: Vec<String> = symbols
            .iter()
            .filter(|s| !matches!(s.visibility, Visibility::Private))
            .map(|s| s.name.clone())
            .collect();
        exports.insert(file_path.clone(), exported_names);

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
                if let Some(resolved) = resolve_specifier(
                    file_path,
                    &reference.context,
                    &known_files,
                    language,
                    workspace_ctx,
                ) {
                    let original_name = reference
                        .context
                        .rsplit("::")
                        .next()
                        .unwrap_or(reference.context.as_str())
                        .to_string();
                    bindings.push(NamedBinding {
                        local_name: reference.name.clone(),
                        original_name,
                        source_file: resolved,
                    });
                }
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
            name: specifier.to_string(),
            kind: ReferenceKind::Import,
            start_line: 1,
            context: String::new(),
            receiver: None,
        }
    }

    fn make_include_ref(specifier: &str) -> RawReference {
        RawReference {
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
                local_name: "MyAlias".to_string(),
                original_name: "OriginalName".to_string(),
                source_file: "src/lib.js".to_string(),
            }],
        );
        let graph = ImportGraph {
            resolved_imports: HashMap::new(),
            exports: HashMap::new(),
            named_bindings: bindings,
        };
        let result = graph.bindings_of("src/main.js");
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].local_name, "MyAlias");
        assert_eq!(result[0].original_name, "OriginalName");
    }

    #[test]
    fn populates_named_bindings_from_import_alias_refs() {
        let make_alias_ref = |alias: &str, specifier: &str| RawReference {
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
        assert_eq!(bindings[0].source_file, "src/config.rs");
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
}
