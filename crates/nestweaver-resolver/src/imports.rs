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
    /// Exact inline module tail after a Rust import consumes its source file.
    pub rust_inline_modules: Option<Vec<String>>,
    /// Import location, used to keep function-local bindings inside their owner.
    pub start_line: u32,
}

/// One file's named bindings, in declaration order, indexed by local name.
///
/// nw-113: a reference looks up the bindings of its own name (or receiver).
/// Scanning every binding in the file per reference was quadratic in the
/// number of imports: 3,000 `import x = require()` lines took seconds.
#[derive(Debug, Default)]
struct FileBindings {
    all: Vec<NamedBinding>,
    /// local name → indices into `all`, ascending.
    by_name: HashMap<String, Vec<usize>>,
}

impl FileBindings {
    fn push(&mut self, binding: NamedBinding) {
        self.by_name
            .entry(binding.local_name.clone())
            .or_default()
            .push(self.all.len());
        self.all.push(binding);
    }

    fn named(&self, name: &str) -> &[usize] {
        self.by_name.get(name).map_or(&[], Vec::as_slice)
    }
}

impl From<Vec<NamedBinding>> for FileBindings {
    fn from(bindings: Vec<NamedBinding>) -> Self {
        let mut file = Self::default();
        for binding in bindings {
            file.push(binding);
        }
        file
    }
}

/// Tracks what each file exports and what it imports.
pub struct ImportGraph {
    /// file → [(specifier, resolved_file)]
    resolved_imports: HashMap<String, Vec<(String, String)>>,
    /// file → [exported_symbol_names]
    exports: HashMap<String, Vec<String>>,
    /// file → [named bindings (aliased imports)]
    named_bindings: HashMap<String, FileBindings>,
    export_aliases: HashMap<String, HashMap<String, (String, Option<String>)>>,
    /// Exact literal-method declaration keyed by terminal exported name.
    literal_export_lines: HashMap<String, HashMap<String, u32>>,
    star_exports: HashMap<String, Vec<String>>,
    go_packages: HashMap<String, String>,
    /// Go importer → the package qualifiers its imports bind.
    go_imports: HashMap<String, GoFileImports>,
    unresolved_exports: HashMap<String, HashSet<String>>,
    incomplete_stars: HashSet<String>,
}

/// What one Go file's imports bind.
///
/// A Go import names a package, which is every non-test file of a directory,
/// and binds it to a qualifier: the import's explicit name, or the package's
/// own name. `calc.New` therefore denotes `New` in ANY file of that package,
/// and a bare `New` denotes no imported file at all unless the package was
/// dot-imported.
#[derive(Debug, Default)]
struct GoFileImports {
    /// qualifier → the package files it names, sorted.
    qualifiers: HashMap<String, Vec<String>>,
    /// Files of packages imported under a qualifier (not dot-imported).
    qualified_files: HashSet<String>,
    /// Files of dot-imported packages: their names are usable bare.
    dot_files: HashSet<String>,
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
            .map_or(&[], |bindings| bindings.all.as_slice())
    }

    /// The named bindings of `file` whose local name is `name`, in
    /// declaration order. An indexed lookup, not a scan of the file.
    pub fn bindings_named<'a>(
        &'a self,
        file: &str,
        name: &str,
    ) -> impl Iterator<Item = &'a NamedBinding> + 'a {
        let (all, indices) = self
            .named_bindings
            .get(file)
            .map_or((&[][..], &[][..]), |bindings| {
                (bindings.all.as_slice(), bindings.named(name))
            });
        indices.iter().map(move |&index| &all[index])
    }

    /// The named bindings of `file` whose local name is `name` or
    /// `receiver`, in declaration order (the order `bindings_of` gives).
    pub fn bindings_named_either<'a>(
        &'a self,
        file: &str,
        name: &str,
        receiver: Option<&str>,
    ) -> Vec<&'a NamedBinding> {
        let Some(bindings) = self.named_bindings.get(file) else {
            return Vec::new();
        };
        let mut indices: Vec<usize> = bindings.named(name).to_vec();
        if let Some(receiver) = receiver.filter(|receiver| *receiver != name) {
            indices.extend_from_slice(bindings.named(receiver));
            indices.sort_unstable();
        }
        indices
            .into_iter()
            .map(|index| &bindings.all[index])
            .collect()
    }

    /// Add an AST-proven same-file inline type route without a file/name fallback.
    pub(crate) fn register_rust_inline_type_imports(
        &mut self,
        file: &str,
        imports: &[nestweaver_parser::parse::ScopedRustInlineTypeImport],
    ) {
        for import in imports {
            let routes = self.resolved_imports.entry(file.into()).or_default();
            if !routes
                .iter()
                .any(|(specifier, target)| specifier == &import.specifier && target == file)
            {
                routes.push((import.specifier.clone(), file.into()));
            }
            let bindings = self.named_bindings.entry(file.into()).or_default();
            let binding = NamedBinding {
                scope: Some(import.scope),
                local_name: import.local_name.clone(),
                original_name: import.original_name.clone(),
                source_file: Some(file.into()),
                rust_inline_modules: Some(import.module_path.clone()),
                start_line: import.line,
            };
            if let Some(existing) = bindings.all.iter_mut().find(|binding| {
                binding.local_name == import.local_name
                    && binding
                        .scope
                        .is_some_and(|scope| scope.position == import.scope.position)
            }) {
                // The AST inline declaration defeats a physical file guess
                // for this use byte; other declarations retain their routes.
                *existing = binding;
            } else {
                bindings.push(binding);
            }
        }
    }

    /// Follow only exact named public exports, bounded like the existing barrel walk.
    /// A public declaration name is not automatically another public export key.
    pub fn exported_target<'a>(
        &'a self,
        file: &'a str,
        name: &'a str,
    ) -> Option<(&'a str, &'a str, usize)> {
        self.exported_target_with_declaration(file, name)
            .map(|(file, name, depth, _)| (file, name, depth))
    }

    pub(crate) fn exported_target_with_declaration<'a>(
        &'a self,
        file: &'a str,
        name: &'a str,
    ) -> Option<(&'a str, &'a str, usize, Option<u32>)> {
        let mut targets = HashMap::new();
        let complete =
            self.collect_export_targets(file, name, 0, &mut HashSet::new(), &mut targets, &mut 256);
        if complete && targets.len() == 1 {
            targets
                .into_iter()
                .next()
                .map(|((file, name, line), depth)| (file, name, depth, line))
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
        targets: &mut HashMap<(&'a str, &'a str, Option<u32>), usize>,
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
                    .entry((
                        file,
                        local,
                        self.literal_export_lines
                            .get(file)
                            .and_then(|lines| lines.get(name))
                            .copied(),
                    ))
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

    /// The package files a Go qualifier (`calc` in `calc.New`) names in
    /// `file`, when `receiver` is one of that file's import qualifiers.
    pub(crate) fn go_qualified_package(&self, file: &str, receiver: &str) -> Option<&[String]> {
        self.go_imports
            .get(file)?
            .qualifiers
            .get(receiver)
            .map(Vec::as_slice)
    }

    /// True when `candidate` belongs only to packages `file` imports under a
    /// qualifier, so an unqualified name in `file` cannot denote it.
    pub(crate) fn go_requires_qualifier(&self, file: &str, candidate: &str) -> bool {
        self.go_imports.get(file).is_some_and(|imports| {
            imports.qualified_files.contains(candidate) && !imports.dot_files.contains(candidate)
        })
    }

    /// The package clause recorded for a Go file, when its references were
    /// part of this resolution pass.
    pub(crate) fn go_package_of(&self, file: &str) -> Option<&str> {
        self.go_packages.get(file).map(String::as_str)
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
    let mut named_bindings: HashMap<String, FileBindings> = HashMap::new();
    let mut export_aliases = HashMap::new();
    let mut literal_export_lines = HashMap::new();
    let mut star_exports = HashMap::new();
    let mut go_packages = HashMap::new();
    let mut go_imports: HashMap<String, GoFileImports> = HashMap::new();
    let mut unresolved_exports: HashMap<String, HashSet<String>> = HashMap::new();
    let mut incomplete_stars = HashSet::new();
    let language_of = |file_path: &str| {
        file_languages
            .and_then(|languages| languages.get(file_path))
            .copied()
            .unwrap_or(fallback_language)
    };

    // Package clauses first: an importer's qualifier is the IMPORTED file's
    // package name, and that file may come later in `files`.
    for (file_path, _, references) in files {
        if language_of(file_path) == Language::Go {
            let packages: Vec<_> = references
                .iter()
                .filter(|reference| reference.kind == ReferenceKind::PackageBinding)
                .collect();
            if packages.len() == 1 {
                go_packages.insert(file_path.clone(), packages[0].name.clone());
            }
        }
    }
    let go_index = files
        .iter()
        .any(|(file_path, _, _)| language_of(file_path) == Language::Go)
        .then(|| lang::go_lang::GoPackageIndex::new(known_files.iter().copied()));
    let jvm_index = files
        .iter()
        .any(|(file_path, _, _)| {
            matches!(language_of(file_path), Language::Kotlin | Language::Java)
        })
        .then(|| lang::kotlin::JvmPackageIndex::new(files, &language_of));
    // Python `from pkg import name` reads the attribute `name` of `pkg` first
    // and imports the submodule `pkg.name` only when no such attribute
    // exists, so the module-level names of each module decide between the
    // two: its module-level declarations, and the names its own module-level
    // imports bind (`from .helpers import helpers` makes `pkg.helpers` the
    // function, not the submodule).
    let python_top_level: HashMap<&str, HashSet<&str>> = files
        .iter()
        .filter(|(file_path, _, _)| language_of(file_path) == Language::Python)
        .map(|(file_path, symbols, references)| {
            let bodies: Vec<(u32, u32)> = symbols
                .iter()
                .filter(|symbol| {
                    !symbol.is_module_owner()
                        && matches!(
                            symbol.kind,
                            nestweaver_schema::SymbolKind::Function
                                | nestweaver_schema::SymbolKind::Method
                                | nestweaver_schema::SymbolKind::Class
                        )
                })
                .map(|symbol| (symbol.start_line, symbol.end_line))
                .collect();
            // A nested `def` has no `parent_name` either; a span strictly
            // inside another body is not module level.
            let nested = |start: u32, end: u32| {
                bodies.iter().any(|&(outer_start, outer_end)| {
                    outer_start <= start
                        && end <= outer_end
                        && (outer_start, outer_end) != (start, end)
                })
            };
            let inside_body = |line: u32| {
                bodies
                    .iter()
                    .any(|&(outer_start, outer_end)| outer_start <= line && line <= outer_end)
            };
            let mut names: HashSet<&str> = symbols
                .iter()
                .filter(|symbol| {
                    symbol.parent_name.is_none()
                        && !symbol.is_module_owner()
                        && !nested(symbol.start_line, symbol.end_line)
                })
                .map(|symbol| symbol.name.as_str())
                .collect();
            names.extend(
                references
                    .iter()
                    .filter(|reference| {
                        reference.kind == ReferenceKind::ImportAlias
                            && !inside_body(reference.start_line)
                    })
                    .map(|reference| reference.name.as_str()),
            );
            (file_path.as_str(), names)
        })
        .collect();

    for (file_path, symbols, references) in files {
        let language = language_of(file_path);
        let literal_methods: HashSet<_> = symbols
            .iter()
            .filter(|symbol| {
                symbol.kind == nestweaver_schema::SymbolKind::Method
                    && symbol.parent_name.as_deref() == Some("module.exports")
            })
            .map(|symbol| (symbol.name.as_str(), symbol.start_line))
            .collect();
        let is_literal_export = |reference: &RawReference| {
            reference.scope.is_some()
                && reference.receiver.is_none()
                && literal_methods.contains(&(reference.context.as_str(), reference.start_line))
        };
        let mut lines = HashMap::new();
        let mut seen = HashSet::new();
        let mut duplicates = HashSet::new();
        let mut literal_names = HashSet::new();
        for reference in references
            .iter()
            .filter(|reference| reference.kind == ReferenceKind::ExportAlias)
        {
            if !seen.insert(reference.name.clone()) {
                duplicates.insert(reference.name.clone());
            }
            if is_literal_export(reference) {
                literal_names.insert(reference.name.clone());
                lines.insert(reference.name.clone(), reference.start_line);
            }
        }
        // The AST traversal does not promise overwrite order. Repeated
        // aliases involving a literal method refuse instead of selecting one.
        for name in duplicates.intersection(&literal_names) {
            lines.remove(name);
            unresolved_exports
                .entry(file_path.clone())
                .or_default()
                .insert(name.clone());
        }
        literal_export_lines.insert(file_path.clone(), lines);
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
                if is_literal_export(reference) {
                    return None;
                }
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
                    let imported = if is_literal_export(reference) {
                        None
                    } else {
                        references.iter().find(|binding| {
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
                    };
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
                let mut resolved = resolve_specifier(
                    file_path,
                    &reference.context,
                    &known_files,
                    language,
                    workspace_ctx,
                );
                let mut original_name = reference
                    .receiver
                    .as_deref()
                    .unwrap_or(&reference.context)
                    .rsplit("::")
                    .next()
                    .unwrap_or(reference.context.as_str())
                    .to_string();
                // `from . import helpers` / `from pkg import helpers as h`
                // binds the SUBMODULE `helpers` unless the package itself
                // defines that name. The binding then denotes the whole
                // module, exactly like `import pkg.helpers as h`.
                if language == Language::Python
                    && original_name != "*"
                    && resolved.as_deref().is_none_or(|package| {
                        !python_top_level
                            .get(package)
                            .is_some_and(|names| names.contains(original_name.as_str()))
                    })
                    && let Some(submodule) = resolve_specifier(
                        file_path,
                        &python_submodule_specifier(&reference.context, &original_name),
                        &known_files,
                        language,
                        workspace_ctx,
                    )
                {
                    resolved = Some(submodule);
                    original_name = "*".into();
                }
                if resolved.is_none()
                    && !matches!(language, Language::JavaScript | Language::TypeScript)
                {
                    continue;
                }
                let rust_inline_modules = if language == Language::Rust {
                    resolved.as_deref().and_then(|file| {
                        lang::rust::inline_type_modules(
                            file_path,
                            &reference.context,
                            file,
                            &known_files,
                        )
                    })
                } else {
                    None
                };
                bindings.push(NamedBinding {
                    rust_inline_modules,
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
            // A Go import names a package (every file of a directory), and a
            // Kotlin/Java import may name a declaration in any file of a
            // package: each target file is recorded.
            if reference.kind == ReferenceKind::Import
                && let Some(go_index) = go_index.as_ref().filter(|_| language == Language::Go)
            {
                let targets = go_index.package_files(specifier, &workspace_ctx.go_modules);
                record_go_import(
                    go_imports.entry(file_path.clone()).or_default(),
                    reference,
                    &targets,
                    &go_packages,
                );
                imports.extend(
                    targets
                        .into_iter()
                        .map(|target| (specifier.clone(), target.to_string())),
                );
                continue;
            }
            if reference.kind == ReferenceKind::Import
                && let Some(jvm_index) = jvm_index
                    .as_ref()
                    .filter(|_| matches!(language, Language::Kotlin | Language::Java))
            {
                let targets = jvm_index.resolve(specifier);
                if !targets.is_empty() {
                    imports.extend(
                        targets
                            .into_iter()
                            .filter(|target| *target != file_path.as_str())
                            .map(|target| (specifier.clone(), target.to_string())),
                    );
                    continue;
                }
            }
            let resolved =
                resolve_specifier(file_path, specifier, &known_files, language, workspace_ctx);
            // A module import consumes the final path segment. An item
            // import leaves the resolved module unchanged from its prefix,
            // even when the item's spelling equals the module filename.
            let rust_module_import = language == Language::Rust
                && resolved.as_ref().is_some_and(|file| {
                    specifier.rsplit_once("::").is_none_or(|(prefix, _)| {
                        lang::rust::resolve_import(file_path, prefix, &known_files).as_ref()
                            != Some(file)
                    })
                });
            if language == Language::Rust
                && reference.kind == ReferenceKind::Import
                && reference.receiver.as_deref() != Some("*")
                && !references.iter().any(|alias| {
                    alias.kind == ReferenceKind::ImportAlias
                        && alias.context == *specifier
                        && alias.start_line == reference.start_line
                })
            {
                let name = specifier.rsplit("::").next().unwrap_or(specifier);
                let rust_inline_modules = resolved.as_deref().and_then(|file| {
                    lang::rust::inline_type_modules(file_path, specifier, file, &known_files)
                });
                bindings.push(NamedBinding {
                    rust_inline_modules,
                    scope: reference.scope,
                    local_name: name.into(),
                    original_name: if rust_module_import { "*" } else { name }.into(),
                    source_file: resolved.clone(),
                    start_line: reference.start_line,
                });
            }
            if let Some(resolved) = resolved {
                imports.push((specifier.clone(), resolved));
            }
        }
        resolved_imports.insert(file_path.clone(), imports);
        if !bindings.is_empty() {
            named_bindings.insert(file_path.clone(), bindings.into());
        }
    }

    follow_python_reexported_bindings(&mut named_bindings, files, &language_of);

    ImportGraph {
        resolved_imports,
        exports,
        named_bindings,
        export_aliases,
        literal_export_lines,
        star_exports,
        go_packages,
        go_imports,
        unresolved_exports,
        incomplete_stars,
    }
}

/// Record the qualifier one Go import binds in its importing file.
fn record_go_import(
    imports: &mut GoFileImports,
    reference: &RawReference,
    targets: &[&str],
    go_packages: &HashMap<String, String>,
) {
    if targets.is_empty() {
        return;
    }
    let qualifier = match reference.receiver.as_deref() {
        // A blank import runs the package's `init` and binds nothing.
        Some("_") => return,
        Some(".") => {
            imports
                .dot_files
                .extend(targets.iter().map(|target| target.to_string()));
            return;
        }
        Some(alias) => alias.to_string(),
        None => {
            // The package clause of the imported files, when they agree;
            // otherwise Go's conventional name for the import path.
            let mut names = targets.iter().filter_map(|target| go_packages.get(*target));
            match names.next() {
                Some(first) if names.all(|name| name == first) => first.clone(),
                _ => lang::go_lang::default_package_name(&reference.name).to_string(),
            }
        }
    };
    imports
        .qualified_files
        .extend(targets.iter().map(|target| target.to_string()));
    let files = imports.qualifiers.entry(qualifier).or_default();
    files.extend(targets.iter().map(|target| target.to_string()));
    files.sort_unstable();
    files.dedup();
}

/// The specifier of submodule `name` of the Python package `package`:
/// `.` + `helpers` is `.helpers`, `pkg.sub` + `helpers` is `pkg.sub.helpers`.
fn python_submodule_specifier(package: &str, name: &str) -> String {
    if package.ends_with('.') {
        format!("{package}{name}")
    } else {
        format!("{package}.{name}")
    }
}

/// Follow Python bindings through a package's re-export.
///
/// `from pkg import compute_alias`, where `pkg/__init__.py` says
/// `from .core import compute as compute_alias`, names `compute` in
/// `pkg/core.py`; the package itself declares nothing of that name. A binding
/// whose source module does not declare its name, but binds it with a
/// module-level import of its own, is rewritten to that import's source.
/// Bounded at three hops; a cycle stops at the hop limit.
fn follow_python_reexported_bindings(
    named_bindings: &mut HashMap<String, FileBindings>,
    files: &[(String, Vec<RawSymbol>, Vec<RawReference>)],
    language_of: &dyn Fn(&str) -> Language,
) {
    let symbols_of: HashMap<&str, &[RawSymbol]> = files
        .iter()
        .filter(|(file_path, _, _)| language_of(file_path) == Language::Python)
        .map(|(file_path, symbols, _)| (file_path.as_str(), symbols.as_slice()))
        .collect();
    // The module-level binding `name` of `module`, when the module declares
    // no top-level symbol `name` itself.
    let reexport = |named_bindings: &HashMap<String, FileBindings>,
                    module: &str,
                    name: &str|
     -> Option<(String, String)> {
        let symbols = symbols_of.get(module)?;
        if symbols
            .iter()
            .any(|symbol| symbol.parent_name.is_none() && symbol.name == name)
        {
            return None;
        }
        let bindings = named_bindings.get(module)?;
        let mut module_level = bindings
            .named(name)
            .iter()
            .map(|&index| &bindings.all[index])
            .filter(|binding| {
                !symbols.iter().any(|symbol| {
                    !symbol.is_module_owner()
                        && matches!(
                            symbol.kind,
                            nestweaver_schema::SymbolKind::Function
                                | nestweaver_schema::SymbolKind::Method
                                | nestweaver_schema::SymbolKind::Class
                        )
                        && symbol.start_line <= binding.start_line
                        && binding.start_line <= symbol.end_line
                })
            });
        let binding = module_level.next()?;
        // Two module-level imports of one name: the later one wins at run
        // time, but which one a reader meant is not evidence enough.
        if module_level.next().is_some() {
            return None;
        }
        Some((binding.source_file.clone()?, binding.original_name.clone()))
    };
    let mut rewrites = Vec::new();
    for (file, bindings) in named_bindings.iter() {
        if !symbols_of.contains_key(file.as_str()) {
            continue;
        }
        for (index, binding) in bindings.all.iter().enumerate() {
            if binding.original_name == "*" {
                continue;
            }
            let Some(mut source) = binding.source_file.clone() else {
                continue;
            };
            let mut original = binding.original_name.clone();
            let mut changed = false;
            for _ in 0..3 {
                if original == "*" {
                    break;
                }
                let Some((next_source, next_original)) =
                    reexport(named_bindings, &source, &original)
                else {
                    break;
                };
                source = next_source;
                original = next_original;
                changed = true;
            }
            if changed {
                rewrites.push((file.clone(), index, source, original));
            }
        }
    }
    for (file, index, source, original) in rewrites {
        if let Some(binding) = named_bindings
            .get_mut(&file)
            .and_then(|bindings| bindings.all.get_mut(index))
        {
            binding.source_file = Some(source);
            binding.original_name = original;
        }
    }
}

/// Path a single import specifier resolves to, when it names a known file.
///
/// Same decision [`build_import_graph`] records for `Import` / `Includes` /
/// `Uses`. Callers that only need that answer for a few files should use this
/// instead of building export maps for every path in the repo.
pub fn resolved_import_path(
    from_file: &str,
    specifier: &str,
    known_files: &HashSet<&str>,
    language: Language,
    workspace_ctx: &WorkspaceContext,
) -> Option<String> {
    resolve_specifier(from_file, specifier, known_files, language, workspace_ctx)
}

/// Every path an import specifier names: all files of a Go package, or the
/// single path [`resolved_import_path`] gives for any other language.
///
/// A file added to a Go package is a new target of every importer of that
/// package, which a single representative path cannot show. Callers asking
/// about many specifiers against one file set should hold one
/// [`ImportTargets`] instead: this builds the Go package index per call.
pub fn resolved_import_paths(
    from_file: &str,
    specifier: &str,
    known_files: &HashSet<&str>,
    language: Language,
    workspace_ctx: &WorkspaceContext,
) -> Vec<String> {
    ImportTargets::new(known_files).paths(from_file, specifier, language, workspace_ctx)
}

/// [`resolved_import_paths`] over one fixed file set, building the Go package
/// index once (on first Go specifier) instead of once per specifier.
pub struct ImportTargets<'a> {
    known_files: &'a HashSet<&'a str>,
    go_index: std::cell::OnceCell<lang::go_lang::GoPackageIndex<'a>>,
}

impl<'a> ImportTargets<'a> {
    pub fn new(known_files: &'a HashSet<&'a str>) -> Self {
        Self {
            known_files,
            go_index: std::cell::OnceCell::new(),
        }
    }

    /// Every path `specifier` names from `from_file`; empty when none.
    pub fn paths(
        &self,
        from_file: &str,
        specifier: &str,
        language: Language,
        workspace_ctx: &WorkspaceContext,
    ) -> Vec<String> {
        if language == Language::Go {
            return self
                .go_index
                .get_or_init(|| {
                    lang::go_lang::GoPackageIndex::new(self.known_files.iter().copied())
                })
                .package_files(specifier, &workspace_ctx.go_modules)
                .into_iter()
                .map(str::to_string)
                .collect();
        }
        resolve_specifier(
            from_file,
            specifier,
            self.known_files,
            language,
            workspace_ctx,
        )
        .into_iter()
        .collect()
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
        Language::Go => lang::go_lang::resolve_import(
            from_file,
            specifier,
            known_files,
            &workspace_ctx.go_modules,
        ),
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
                rust_inline_modules: None,
                scope: None,
                local_name: "MyAlias".to_string(),
                original_name: "OriginalName".to_string(),
                source_file: Some("src/lib.js".to_string()),
                start_line: 1,
            }],
        );
        let graph = ImportGraph {
            literal_export_lines: HashMap::new(),
            resolved_imports: HashMap::new(),
            exports: HashMap::new(),
            named_bindings: bindings
                .into_iter()
                .map(|(file, bindings)| (file, bindings.into()))
                .collect(),
            export_aliases: HashMap::new(),
            star_exports: HashMap::new(),
            go_packages: HashMap::new(),
            go_imports: HashMap::new(),
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

    /// nw-113 item 15: the by-name lookups return exactly the bindings a
    /// scan of `bindings_of` would, in the same declaration order.
    #[test]
    fn indexed_binding_lookups_match_a_scan_in_order() {
        let binding = |local: &str, line: u32| NamedBinding {
            rust_inline_modules: None,
            scope: None,
            local_name: local.into(),
            original_name: "*".into(),
            source_file: Some("lib.ts".into()),
            start_line: line,
        };
        let graph = ImportGraph {
            literal_export_lines: HashMap::new(),
            resolved_imports: HashMap::new(),
            exports: HashMap::new(),
            named_bindings: HashMap::from([(
                "a.ts".to_string(),
                vec![
                    binding("m", 1),
                    binding("f", 2),
                    binding("other", 3),
                    binding("m", 4),
                    binding("f", 5),
                ]
                .into(),
            )]),
            export_aliases: HashMap::new(),
            star_exports: HashMap::new(),
            go_packages: HashMap::new(),
            go_imports: HashMap::new(),
            unresolved_exports: HashMap::new(),
            incomplete_stars: HashSet::new(),
        };
        let lines = |bindings: Vec<&NamedBinding>| -> Vec<u32> {
            bindings.iter().map(|binding| binding.start_line).collect()
        };
        assert_eq!(lines(graph.bindings_named("a.ts", "m").collect()), [1, 4]);
        assert_eq!(
            lines(graph.bindings_named_either("a.ts", "f", Some("m"))),
            [1, 2, 4, 5]
        );
        assert_eq!(
            lines(graph.bindings_named_either("a.ts", "m", Some("m"))),
            [1, 4]
        );
        assert_eq!(
            lines(graph.bindings_named_either("a.ts", "f", None)),
            [2, 5]
        );
        assert!(graph.bindings_named("b.ts", "m").next().is_none());
        assert_eq!(graph.bindings_of("a.ts").len(), 5);
    }

    #[test]
    fn review_star_walk_budget_fails_closed_after_a_known_target() {
        let mut graph = ImportGraph {
            literal_export_lines: HashMap::new(),
            resolved_imports: HashMap::new(),
            exports: HashMap::new(),
            named_bindings: HashMap::new(),
            go_packages: HashMap::new(),
            go_imports: HashMap::new(),
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
            literal_export_lines: HashMap::new(),
            resolved_imports: HashMap::new(),
            exports: HashMap::new(),
            named_bindings: HashMap::new(),
            go_packages: HashMap::new(),
            go_imports: HashMap::new(),
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
