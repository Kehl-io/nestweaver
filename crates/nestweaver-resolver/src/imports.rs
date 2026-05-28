use std::collections::HashMap;

use nestweaver_parser::{RawReference, RawSymbol, ReferenceKind};
use nestweaver_schema::{Language, Visibility};

use crate::lang;
use crate::workspace::WorkspaceContext;

/// Tracks an aliased import binding (e.g., `import { X as Y }`).
///
/// Currently a placeholder — `named_bindings` in `ImportGraph` is not yet
/// populated by the parser. The infrastructure is in place for future
/// extraction of import aliases from .scm query patterns.
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
    let known_files: Vec<&str> = files.iter().map(|(path, _, _)| path.as_str()).collect();

    let mut exports: HashMap<String, Vec<String>> = HashMap::new();
    let mut resolved_imports: HashMap<String, Vec<(String, String)>> = HashMap::new();

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
        for reference in references {
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
    }

    ImportGraph {
        resolved_imports,
        exports,
        named_bindings: HashMap::new(),
    }
}

fn resolve_specifier(
    from_file: &str,
    specifier: &str,
    known_files: &[&str],
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
        Language::Cpp | Language::Rust => None,
        Language::C => lang::c::resolve_import(from_file, specifier, known_files),
        Language::CSharp => lang::csharp::resolve_import(from_file, specifier, known_files),
        Language::Kotlin => lang::kotlin::resolve_import(from_file, specifier, known_files),
        Language::Php => lang::php::resolve_import(from_file, specifier, known_files),
        Language::Ruby => lang::ruby::resolve_import(from_file, specifier, known_files),
        Language::Dart => lang::dart::resolve_import(from_file, specifier, known_files),
        Language::Swift => lang::swift::resolve_import(from_file, specifier, known_files),
        Language::Cobol
        | Language::Lua
        | Language::Bash
        | Language::Scala
        | Language::Elixir
        | Language::Zig
        | Language::ObjectiveC
        | Language::Groovy
        | Language::PowerShell
        | Language::Julia
        | Language::Sql
        | Language::Hcl
        | Language::Fortran
        | Language::Pascal
        | Language::SystemVerilog => None,
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
            signature: format!("function {name}()"),
            content_hash: String::new(),
            is_entry_point: false,
            entry_point_kind: None,
            visibility: Visibility::Inferred,
            type_info: None,
        }
    }

    fn make_import_ref(specifier: &str) -> RawReference {
        RawReference {
            name: specifier.to_string(),
            kind: ReferenceKind::Import,
            start_line: 1,
            context: String::new(),
        }
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
