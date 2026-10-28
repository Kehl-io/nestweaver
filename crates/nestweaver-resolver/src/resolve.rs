use nestweaver_parser::{RawReference, RawSymbol, ReferenceKind};
use nestweaver_schema::{
    EdgeEvidence, EdgeType, Language, MatchType, ResolvedEdge, SymbolKind, Visibility,
    confidence_score, symbol_uid,
};
use rayon::prelude::*;

use crate::imports::{ImportGraph, JvmImportRank, NamedBinding, build_import_graph_with_languages};
use crate::type_extractors::TypeNameParts;
use crate::types::TypeEnvironment;
use crate::util::parent_dir;
use crate::workspace::WorkspaceContext;

/// Resolve all references across files into `ResolvedEdge`s.
///
/// Three-pass approach:
/// 1. Build the import graph (what each file imports/exports)
/// 2. For each non-import reference, find the target symbol using priority:
///    - Same file → SameFileExact confidence (a local symbol shadows an
///      import alias of the same name)
///    - Import alias (`use a::b as c`) → the original name in the binding's
///      source file → ImportResolved confidence
///    - Direct imports → ImportResolved confidence
///    - Re-exports (one level deep) → ReExportResolved confidence
///    - Same package/directory → SamePackageFallback confidence
///    - No match → confidence 0.0, target_uid = "unresolved:{name}"
/// 3. Attribute IMPORTS to genuine resolved binding users or explicitly named
///    Rust imports enclosed by source symbols.
///
/// Edges are deduplicated by (source_uid, target_uid, edge_type).
///
/// The optional `workspace_ctx` enables resolution of monorepo workspace
/// package imports and tsconfig path aliases for JS/TS files.
///
/// # LIMITATION: Cross-repo edge resolution
///
/// This resolver processes a single repo at a time (`repo_uid`). Edges are
/// only created between symbols within the same repo. Cross-repo edges (e.g.
/// repo B calling a function exported by repo A) are **not** created because
/// the resolver does not have access to other repos' symbol tables during a
/// single-repo indexing pass.
///
/// To support cross-repo edges, a second resolution pass would need to:
/// 1. Collect all "unresolved:{name}" targets that match package imports
/// 2. Look up exported symbols from other indexed repos in the store
/// 3. Create CALLS/IMPORTS edges across repo boundaries
///
/// Until then, cross-boundary impact analysis relies on the hybrid client's
/// two-tier and continuation routing to stitch results at query time rather
/// than at index time.
pub fn resolve_references(
    files: &[(String, Vec<RawSymbol>, Vec<RawReference>)],
    language: Language,
    repo_uid: &str,
) -> Vec<ResolvedEdge> {
    resolve_references_with_context(
        files,
        language,
        repo_uid,
        &WorkspaceContext::default(),
        None,
        None,
    )
}

/// Like `resolve_references` but with an explicit `WorkspaceContext` for
/// monorepo workspace package and tsconfig path alias resolution.
///
/// When `type_envs` is `Some`, per-file `TypeEnvironment` data is available
/// for type-aware call resolution. Passing `None` preserves the previous
/// behaviour.
pub fn resolve_references_with_context(
    files: &[(String, Vec<RawSymbol>, Vec<RawReference>)],
    language: Language,
    repo_uid: &str,
    workspace_ctx: &WorkspaceContext,
    _type_envs: Option<&std::collections::HashMap<String, crate::types::TypeEnvironment>>,
    resolve_only: Option<&std::collections::HashSet<String>>,
) -> Vec<ResolvedEdge> {
    resolve_references_with_file_languages(
        files,
        language,
        repo_uid,
        workspace_ctx,
        _type_envs,
        resolve_only,
        None,
    )
}

type ScopedCallKey<'a> = (&'a str, Option<&'a str>, usize);

/// Per method `(file, start_line)`, the callee names of its
/// `return Name(..)` / `return new Name(..)` / `return cls(..)` statements.
/// The only evidence the unique-method fallback accepts for what an
/// unannotated method returns (see `fallback_receiver_type`).
type ReturnedConstructions = std::collections::HashMap<(String, u32), Vec<String>>;

/// Whether a call reference's evidence line is `return Name(..)` or
/// `return new Name(..)` for that very callee: the call IS the returned
/// value, not an argument to it (`return Optional.of(new A())`), an element
/// of it (`return [new Q()]`) or a receiver in it (`return A().b()`).
fn returns_construction_of(context: &str, name: &str) -> bool {
    let Some(rest) = context.trim_start().strip_prefix("return") else {
        return false;
    };
    if !rest.starts_with(char::is_whitespace) {
        return false;
    }
    let rest = rest.trim_start();
    let rest = rest.strip_prefix("new ").map_or(rest, str::trim_start);
    rest.strip_prefix(name).is_some_and(|after| {
        let after = after.trim_start();
        let Some(close) = after
            .starts_with('(')
            .then(|| matching_close_paren(after))
            .flatten()
        else {
            return false;
        };
        // Nothing may follow the argument list but the statement's end.
        matches!(after[close + 1..].trim(), "" | ";")
    })
}

/// Byte index of the `)` closing the `(` that `text` starts with.
fn matching_close_paren(text: &str) -> Option<usize> {
    let mut depth = 0i32;
    for (index, ch) in text.char_indices() {
        match ch {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(index);
                }
            }
            _ => {}
        }
    }
    None
}

fn returned_constructions(
    files: &[(String, Vec<RawSymbol>, Vec<RawReference>)],
    sorted_symbols_per_file: &[Vec<&RawSymbol>],
) -> ReturnedConstructions {
    let mut map = ReturnedConstructions::new();
    for ((file, _, references), sorted_syms) in files.iter().zip(sorted_symbols_per_file) {
        for reference in references {
            if reference.kind != ReferenceKind::Call
                || reference.receiver.is_some()
                || !returns_construction_of(&reference.context, &reference.name)
            {
                continue;
            }
            if let Enclosing::Exact(symbol) =
                find_enclosing_symbol(sorted_syms, reference.start_line)
                && symbol.kind == SymbolKind::Method
            {
                map.entry((file.clone(), symbol.start_line))
                    .or_default()
                    .push(reference.name.clone());
            }
        }
    }
    map
}

/// Index exact AST call identities once per file. Retain duplicate references
/// so an ambiguous assignment stays refused instead of taking an arbitrary call.
fn scoped_call_reference_index<'a>(
    references: impl IntoIterator<Item = &'a RawReference>,
) -> std::collections::HashMap<ScopedCallKey<'a>, Vec<&'a RawReference>> {
    let mut calls: std::collections::HashMap<_, Vec<_>> = Default::default();
    for reference in references {
        if reference.kind == ReferenceKind::Call
            && let Some(scope) = reference.scope
        {
            calls
                .entry((
                    reference.name.as_str(),
                    reference.receiver.as_deref(),
                    scope.position,
                ))
                .or_default()
                .push(reference);
        }
    }
    calls
}

/// Resolve a mixed-language repository using the language of each source file.
/// The fallback preserves the single-language API for callers without a file map.
pub fn resolve_references_with_file_languages(
    files: &[(String, Vec<RawSymbol>, Vec<RawReference>)],
    fallback_language: Language,
    repo_uid: &str,
    workspace_ctx: &WorkspaceContext,
    _type_envs: Option<&std::collections::HashMap<String, crate::types::TypeEnvironment>>,
    resolve_only: Option<&std::collections::HashSet<String>>,
    file_languages: Option<&std::collections::HashMap<String, Language>>,
) -> Vec<ResolvedEdge> {
    let language_for = |file: &str| {
        file_languages
            .and_then(|languages| languages.get(file))
            .copied()
            .unwrap_or(fallback_language)
    };
    let mut graph =
        build_import_graph_with_languages(files, fallback_language, file_languages, workspace_ctx);
    if let Some(envs) = _type_envs {
        for (file, env) in envs {
            if language_for(file) == Language::Rust {
                graph.register_rust_inline_type_imports(file, &env.rust_inline_type_imports);
            }
        }
    }

    // Pre-sort symbols per file so find_enclosing_symbol's binary search invariant holds.
    // Tree-sitter guarantees sorted output in production, but callers (e.g. property tests)
    // may pass unsorted symbols, so we sort defensively here once per call.
    let sorted_symbols_per_file: Vec<Vec<&RawSymbol>> = files
        .iter()
        .map(|(_, symbols, _)| {
            let mut v: Vec<&RawSymbol> = symbols.iter().collect();
            // nw-113: the `<module>` owner sorts ahead of any symbol on its
            // line, so it is always first whatever order the symbols arrive
            // in (the parser's, or the store's when `build_reresolve_edges`
            // rebuilds them). `resolve_single_reference` relies on that to
            // find it in O(1).
            v.sort_by_key(|s| (s.start_line, !s.is_module_owner()));
            debug_assert!(
                v.iter().skip(1).all(|s| !s.is_module_owner()),
                "the <module> owner must sort first"
            );
            v
        })
        .collect();

    // Build a lookup: symbol_name → Vec<(file_path, RawSymbol)>
    let mut symbol_map: std::collections::HashMap<String, Vec<(&str, &RawSymbol)>> =
        std::collections::HashMap::new();
    for (file_path, symbols, _) in files {
        for sym in symbols {
            symbol_map
                .entry(sym.name.clone())
                .or_default()
                .push((file_path.as_str(), sym));
        }
    }

    let returned_constructions = returned_constructions(files, &sorted_symbols_per_file);
    let php_functions =
        crate::php::PhpFunctions::build(files, |file| language_for(file) == Language::Php);

    // Build a parent-type lookup from Extends references for MRO walk.
    // Maps child type name → list of parent type names.
    let extends_map: std::collections::HashMap<String, Vec<String>> = {
        let mut map: std::collections::HashMap<String, Vec<String>> =
            std::collections::HashMap::new();
        for ((_, _, references), sorted_syms) in files.iter().zip(sorted_symbols_per_file.iter()) {
            for reference in references {
                if reference.kind == ReferenceKind::Extends
                    && let Some(sym) =
                        find_enclosing_symbol(sorted_syms, reference.start_line).symbol()
                    // nw-330: `Extension` belongs here. A Rust `impl Trait
                    // for Type` block IS the symbol that encloses the `Extends`
                    // reference to the trait, and it used to be
                    // `SymbolKind::Class`. Giving impl blocks their own kind
                    // without widening this filter would have silently deleted
                    // every Rust trait relationship from the MRO map — a
                    // modelling fix quietly losing the one structural fact the
                    // model already had.
                    && matches!(
                        sym.kind,
                        SymbolKind::Class
                            | SymbolKind::Enum
                            | SymbolKind::Interface
                            | SymbolKind::Trait
                            | SymbolKind::Extension
                    )
                {
                    map.entry(sym.name.clone())
                        .or_default()
                        .push(reference.name.clone());
                }
            }
        }
        map
    };

    // Scalar Rust factory results are seeded only after resolving the exact
    // AST free-call reference to one target declaration. Sibling function names
    // and the engine's line-global return map cannot donate a scoped type.
    let mut scoped_envs = _type_envs.cloned();
    if let (Some(original_envs), Some(derived)) = (_type_envs, scoped_envs.as_mut()) {
        let mut seeds = Vec::new();
        let returns_by_uid: std::collections::HashMap<_, _> = files
            .iter()
            .flat_map(|(file, symbols, _)| {
                symbols
                    .iter()
                    .filter(|symbol| {
                        matches!(symbol.kind, SymbolKind::Function | SymbolKind::Method)
                    })
                    .map(move |symbol| {
                        (
                            symbol_uid(repo_uid, file, &symbol.name, symbol.start_line),
                            (file, symbol),
                        )
                    })
            })
            .collect();
        for ((file_path, _, references), sorted_syms) in files.iter().zip(&sorted_symbols_per_file)
        {
            if !matches!(language_for(file_path), Language::Rust | Language::Go) {
                continue;
            }
            let Some(env) = original_envs.get(file_path) else {
                continue;
            };
            if env.call_assignments.is_empty()
                && env.go_ranges.iter().all(|range| {
                    !matches!(
                        range.source,
                        nestweaver_parser::parse::GoRangeSource::Call(_)
                    )
                })
            {
                continue;
            }
            let local_bindings: Vec<_> = references
                .iter()
                .filter(|reference| reference.kind == ReferenceKind::LocalBinding)
                .collect();
            let scope_index = FileScopeIndex::build(&local_bindings, sorted_syms);
            let calls_by_identity = scoped_call_reference_index(references);
            for (assignment, range_value) in env
                .call_assignments
                .iter()
                .map(|assignment| (assignment, false))
                .chain(
                    env.go_ranges
                        .iter()
                        .filter_map(|range| match &range.source {
                            nestweaver_parser::parse::GoRangeSource::Call(call) => {
                                Some((call.as_ref(), true))
                            }
                            _ => None,
                        }),
                )
            {
                let Some(calls) = calls_by_identity.get(&(
                    assignment.callee.as_str(),
                    assignment.receiver.as_deref(),
                    assignment.call_position,
                )) else {
                    continue;
                };
                if calls.len() != 1 {
                    continue;
                }
                if assignment.receiver.is_none()
                    && assignment.local_callee_exists
                    && assignment.local_callee_line.is_none()
                {
                    continue;
                }
                let Some(edge) = resolve_single_reference(
                    file_path,
                    calls[0],
                    &local_bindings,
                    &scope_index,
                    sorted_syms,
                    &symbol_map,
                    &extends_map,
                    &graph,
                    language_for(file_path),
                    repo_uid,
                    _type_envs,
                    &returned_constructions,
                    &php_functions,
                ) else {
                    continue;
                };
                if !edge.target_uid.starts_with("sym:") {
                    continue;
                }
                let exact_local = assignment
                    .local_callee_line
                    .map(|line| symbol_uid(repo_uid, file_path, &assignment.callee, line));
                let target_uid = if assignment.receiver.is_none() {
                    exact_local.as_ref().unwrap_or(&edge.target_uid)
                } else {
                    &edge.target_uid
                };
                let Some((target_file, target)) = returns_by_uid.get(target_uid).copied() else {
                    continue;
                };
                let asynchronous = target
                    .signature
                    .split_whitespace()
                    .any(|token| token == "async");
                if assignment.awaited != asynchronous {
                    continue;
                }
                // Unchanged Go callees may be outside the incremental type-
                // environment scope. Their exact parsed declaration still
                // carries its scalar return type; no name-only lookup is used.
                let go_return = (language_for(file_path) == Language::Go)
                    .then(|| {
                        let raw = target.type_info.as_ref()?.return_type.as_deref()?;
                        Some(crate::type_extractors::TypeBinding {
                            type_name: raw.trim().trim_start_matches('*').trim().to_string(),
                            line: target.start_line,
                            confidence: 0.95,
                            source: crate::type_extractors::BindingSource::ReturnType,
                        })
                    })
                    .flatten();
                let Some(binding) = original_envs
                    .get(target_file)
                    .and_then(|env| env.ast_return_type(&target.name, target.start_line))
                    .or(go_return.as_ref())
                else {
                    continue;
                };
                let mut binding = binding.clone();
                if range_value {
                    let Some(element) = crate::types::go_slice_element(&binding.type_name) else {
                        continue;
                    };
                    binding.type_name = element;
                }
                seeds.push((
                    file_path.clone(),
                    assignment.clone(),
                    target_file.clone(),
                    binding,
                ));
            }
        }
        for (file, assignment, target_file, binding) in seeds {
            if let Some(env) = derived.get_mut(&file) {
                env.seed_scoped_return(
                    &assignment,
                    &target_file,
                    &binding,
                    language_for(&file) == Language::Go,
                );
            }
        }
        for env in derived.values_mut() {
            env.propagate_scoped_aliases();
            env.propagate_go_ranges();
        }
    }
    let type_envs = scoped_envs.as_ref().or(_type_envs);

    // ── Pass 2: Resolve non-import references in parallel per file ─────
    let ref_edges: Vec<ResolvedEdge> = files
        .par_iter()
        .zip(sorted_symbols_per_file.par_iter())
        .flat_map(|((file_path, _symbols, references), sorted_syms)| {
            // When resolve_only is set, skip files outside the filter.
            // The symbol index and import graph are still built from ALL files
            // so references from filtered files can find targets anywhere.
            if let Some(filter) = resolve_only
                && !filter.contains(file_path)
            {
                return Vec::new();
            }
            let mut local_edges = Vec::new();
            let language = language_for(file_path);
            let local_bindings: Vec<&RawReference> = references
                .iter()
                .filter(|reference| {
                    reference.kind == ReferenceKind::LocalBinding
                        && !graph
                            .bindings_named(file_path, &reference.name)
                            .any(|binding| {
                                if let Some(scope) = reference.scope {
                                    binding
                                        .scope
                                        .is_some_and(|import| import.position == scope.position)
                                } else {
                                    binding.start_line == reference.start_line
                                }
                            })
                })
                .collect();
            let scope_index =
                FileScopeIndex::build(&local_bindings, sorted_syms).with_hoisted(references);
            for reference in references {
                if let Some(edge) = resolve_single_reference(
                    file_path,
                    reference,
                    &local_bindings,
                    &scope_index,
                    sorted_syms,
                    &symbol_map,
                    &extends_map,
                    &graph,
                    language,
                    repo_uid,
                    type_envs,
                    &returned_constructions,
                    &php_functions,
                ) {
                    if edge.target_uid.starts_with("sym:")
                        && edge.evidence.iter().any(|evidence| {
                            matches!(
                                evidence.kind.as_str(),
                                "import_alias"
                                    | "import_resolved"
                                    | "re_export"
                                    | "reexport_resolved"
                            )
                        })
                    {
                        let mut import = edge.clone();
                        import.edge_type = EdgeType::Imports;
                        local_edges.push(import);
                    }
                    if language == Language::Go {
                        local_edges.extend(go_build_variant_edges(
                            file_path,
                            &reference.name,
                            &edge,
                            &symbol_map,
                            &graph,
                            repo_uid,
                        ));
                    }
                    local_edges.push(edge);
                }
            }
            local_edges
        })
        .collect();

    let mut edges: Vec<ResolvedEdge> = ref_edges;

    // A named Rust use inside a genuine owner is a precise dependency,
    // including type-only uses. Module imports never fan out to all exports.
    for ((file, _, references), sorted) in files.iter().zip(&sorted_symbols_per_file) {
        if language_for(file) != Language::Rust
            || resolve_only.is_some_and(|filter| !filter.contains(file))
        {
            continue;
        }
        for reference in references
            .iter()
            .filter(|reference| reference.kind == ReferenceKind::Import)
        {
            let Some(source) = find_enclosing_symbol(sorted, reference.start_line).symbol() else {
                continue;
            };
            let Some(name) = reference.name.rsplit("::").next() else {
                continue;
            };
            for (specifier, target_file) in graph.imports_of(file) {
                if specifier != reference.name {
                    continue;
                }
                let Some(candidates) = symbol_map.get(name) else {
                    continue;
                };
                let mut targets = candidates.iter().filter(|(path, symbol)| {
                    *path == target_file
                        && symbol.visibility != Visibility::Private
                        && symbol.name == name
                });
                let Some((path, target)) = targets.next() else {
                    continue;
                };
                if targets.next().is_some() {
                    continue;
                }
                let confidence = confidence_score(MatchType::ImportResolved, Language::Rust);
                edges.push(ResolvedEdge {
                    source_uid: symbol_uid(repo_uid, file, &source.name, source.start_line),
                    target_uid: symbol_uid(repo_uid, path, &target.name, target.start_line),
                    edge_type: EdgeType::Imports,
                    confidence,
                    link_type: None,
                    evidence: vec![EdgeEvidence {
                        kind: "structural".into(),
                        weight: confidence,
                        note: None,
                    }],
                });
            }
        }
    }

    // ── Deduplicate edges by (source_uid, target_uid, edge_type) ──────
    {
        let mut seen = std::collections::HashSet::new();
        edges.retain(|e| seen.insert((e.source_uid.clone(), e.target_uid.clone(), e.edge_type)));
    }

    edges
}

/// Go build-variant siblings of a resolved Go target.
///
/// Two declarations of one top-level name (or one method on one receiver
/// type) in the same Go package only compile when build constraints keep them
/// out of the same build: `//go:build js` in `node_js.go` and `!js` in
/// `node_net.go`, or `_linux`/`_windows`/`_amd64` file suffixes. The tiers
/// above pick ONE of them (the same-package tier sorts by path), so the call in
/// `node.go` linked only `node_js.go`'s variant and the other was reported dead
/// and had an empty blast radius. A call site calls whichever variant the
/// build selects, so every variant is a callee. The parser does not record
/// build constraints, and does not need to: the compiler already guarantees a
/// same-package duplicate is a build variant.
fn go_build_variant_edges(
    file_path: &str,
    name: &str,
    edge: &ResolvedEdge,
    symbol_map: &std::collections::HashMap<String, Vec<(&str, &RawSymbol)>>,
    graph: &ImportGraph,
    repo_uid: &str,
) -> Vec<ResolvedEdge> {
    if edge.edge_type != EdgeType::Calls || !edge.target_uid.starts_with("sym:") {
        return Vec::new();
    }
    // The target's name is not on the edge; recover the symbol by UID among
    // the reference's same-named candidates (an aliased import binds another
    // name and simply finds nothing here).
    let Some(siblings) = symbol_map.get(name) else {
        return Vec::new();
    };
    let Some((target_file, target)) = siblings.iter().copied().find(|(file, symbol)| {
        symbol_uid(repo_uid, file, &symbol.name, symbol.start_line) == edge.target_uid
    }) else {
        return Vec::new();
    };
    // A declaration in the calling file is the one this build compiles with
    // that file; the same-file tier already chose it.
    if target_file == file_path || !matches!(target.kind, SymbolKind::Function | SymbolKind::Method)
    {
        return Vec::new();
    }
    siblings
        .iter()
        .filter(|(file, symbol)| {
            *file != target_file
                && *file != file_path
                && symbol.kind == target.kind
                && symbol.parent_name == target.parent_name
                && parent_dir(file) == parent_dir(target_file)
                && go_same_package(graph, file, target_file)
        })
        .map(|(file, symbol)| ResolvedEdge {
            source_uid: edge.source_uid.clone(),
            target_uid: symbol_uid(repo_uid, file, &symbol.name, symbol.start_line),
            edge_type: edge.edge_type,
            confidence: edge.confidence,
            link_type: None,
            evidence: vec![EdgeEvidence {
                kind: "go_build_variant".to_string(),
                weight: edge.confidence,
                note: Some(format!("build variant of {target_file}")),
            }],
        })
        .collect()
}

/// Whether two Go files in one directory are in one package. An incremental
/// re-resolve passes unchanged files without references, so their package
/// clause is unknown; then the only legal same-directory split is an
/// external `_test` package, so the same directory counts as the same
/// package unless exactly one of the two is a `_test.go` file.
fn go_same_package(graph: &ImportGraph, a: &str, b: &str) -> bool {
    // A `//go:build ignore` file is a standalone `go run` program: it shares
    // a package with no other file of its directory (not a build variant).
    if graph.go_build_ignored(a) || graph.go_build_ignored(b) {
        return a == b;
    }
    match (graph.go_package_of(a), graph.go_package_of(b)) {
        (Some(left), Some(right)) => left == right,
        _ => a.ends_with("_test.go") == b.ends_with("_test.go"),
    }
}

/// Resolve a single non-import reference to a `ResolvedEdge`, or return `None`
/// if the reference should be skipped (e.g. Import/Uses kind, no enclosing symbol).
///
/// This is extracted from the main loop body so it can be called from parallel
/// iterators without needing labeled `continue` across closure boundaries.
#[allow(clippy::too_many_arguments)]
fn resolve_single_reference(
    file_path: &str,
    reference: &RawReference,
    local_bindings: &[&RawReference],
    scope_index: &FileScopeIndex<'_>,
    sorted_syms: &[&RawSymbol],
    symbol_map: &std::collections::HashMap<String, Vec<(&str, &RawSymbol)>>,
    extends_map: &std::collections::HashMap<String, Vec<String>>,
    graph: &ImportGraph,
    language: Language,
    repo_uid: &str,
    type_envs: Option<&std::collections::HashMap<String, crate::types::TypeEnvironment>>,
    returned_constructions: &ReturnedConstructions,
    php_functions: &crate::php::PhpFunctions<'_>,
) -> Option<ResolvedEdge> {
    let edge_type = match reference.kind {
        ReferenceKind::Call | ReferenceKind::Macro => EdgeType::Calls,
        ReferenceKind::Extends => EdgeType::Extends,
        ReferenceKind::Implements => EdgeType::Implements,
        ReferenceKind::Includes => EdgeType::Includes,
        ReferenceKind::TypeRef => EdgeType::Uses,
        ReferenceKind::ReadAccess | ReferenceKind::WriteAccess => EdgeType::Accesses,
        ReferenceKind::Import
        | ReferenceKind::ImportAlias
        | ReferenceKind::PackageBinding
        | ReferenceKind::Uses
        | ReferenceKind::LocalBinding
        | ReferenceKind::HoistedDeclaration
        | ReferenceKind::PhpNamespace
        | ReferenceKind::PhpFunctionImport
        | ReferenceKind::PhpNamespaceAlias
        | ReferenceKind::PhpFunctionCall
        | ReferenceKind::ExportAlias => return None,
    };

    // nw-349 (1). The three cases are now distinguishable, and only two of them
    // name a source:
    //
    //   Exact      — a real span contains this line. Trustworthy.
    //   Degenerate — no span contains it; the nearest preceding CODE-BEARING
    //                one-line symbol is the best guess. Still a guess, but it can
    //                no longer be a `Constant`/`Variable`/`Property`, which is
    //                what it frequently was.
    //   ModuleScope — the line belongs to no symbol. There is no source symbol to
    //                name, and this pass declines to invent one. See the note on
    //                `Enclosing` for why the obvious candidates were rejected.
    //
    // nw-113: in a file that has a `<module>` owner (the parser minted one
    // because a call there sits outside every code-bearing span), a CALL at
    // module scope, or one the degenerate fallback would pin on a one-line
    // function above it, belongs to the module (CodeQL: JS `TopLevel`, Python
    // `Module`). In JS/TS/Python/PHP spans are exact, so a degenerate guess
    // there is always module-level code. Other reference kinds keep today's
    // behaviour: the owner is for executed calls, not type mentions or field
    // reads.
    // The owner always sorts first (asserted where `sorted_syms` is built).
    let module_owner = || {
        sorted_syms
            .first()
            .copied()
            .filter(|s| edge_type == EdgeType::Calls && s.is_module_owner())
    };
    let source_sym = match find_enclosing_symbol(sorted_syms, reference.start_line) {
        Enclosing::Exact(s) => s,
        Enclosing::Degenerate(s) => module_owner().unwrap_or(s),
        Enclosing::ModuleScope => module_owner()?,
    };
    let source_uid = symbol_uid(repo_uid, file_path, &source_sym.name, source_sym.start_line);

    if language == Language::Php
        && let Some(target) = php_functions.resolve(file_path, reference)
    {
        return Some(match target {
            Some((target_file, symbol)) => {
                let confidence = confidence_score(
                    if target_file == file_path {
                        MatchType::SameFileExact
                    } else {
                        MatchType::ImportResolved
                    },
                    language,
                );
                ResolvedEdge {
                    source_uid,
                    target_uid: symbol_uid(repo_uid, target_file, &symbol.name, symbol.start_line),
                    edge_type,
                    confidence,
                    link_type: None,
                    evidence: vec![EdgeEvidence {
                        kind: "php_namespace_function".into(),
                        weight: confidence,
                        note: None,
                    }],
                }
            }
            None => unresolved_edge(source_uid, &reference.name, edge_type),
        });
    }

    // nw-724: a parameter, let, or lambda binding is the target. Do not
    // continue on to a same-named function in this file or another.
    if reference.receiver.is_none()
        && name_is_locally_bound(
            local_bindings,
            scope_index,
            sorted_syms,
            reference,
            language,
        )
    {
        return Some(unresolved_edge(source_uid, &reference.name, edge_type));
    }

    // A declared class field with an unknown value cannot borrow a type from
    // an imported file sharing its spelling (`this.store` vs `store.ts`).
    if edge_type == EdgeType::Calls
        && matches!(language, Language::JavaScript | Language::TypeScript)
        && let Some(field) = reference
            .receiver
            .as_deref()
            .and_then(|receiver| receiver.strip_prefix("this."))
        && let Some(owner) = source_sym.parent_name.as_deref()
        && symbol_map.get(field).is_some_and(|symbols| {
            let properties: Vec<_> = symbols
                .iter()
                .filter(|(file, symbol)| {
                    *file == file_path
                        && symbol.kind == SymbolKind::Property
                        && symbol.parent_name.as_deref() == Some(owner)
                })
                .collect();
            !properties.is_empty()
                && properties
                    .iter()
                    .filter(|(_, symbol)| {
                        symbol
                            .type_info
                            .as_ref()
                            .and_then(|info| info.declared_type.as_ref())
                            .is_some()
                    })
                    .count()
                    != 1
        })
    {
        return Some(unresolved_edge(source_uid, &reference.name, edge_type));
    }

    // ── Type-aware resolution for member calls with known receiver type ──
    let file_env = type_envs.and_then(|envs| envs.get(file_path));
    let receiver_inference = ReceiverInference {
        file_path,
        reference,
        scope_index,
        symbol_map,
        extends_map,
        graph,
        language,
        env: file_env,
    };
    // A receiver that is itself a call or `new` expression (`new K().m2()`,
    // `K.make().m2()`, `k.m1().m2()`, `New(1).Add()`) has no binding of its
    // own. Its type comes from the constructed type or the callee's declared
    // return type, followed through the chain.
    let inferred_receiver = if edge_type == EdgeType::Calls
        && let Some(receiver) = reference.receiver.as_deref()
        && is_expression_receiver(receiver)
    {
        receiver_inference
            .infer(receiver, 0)
            .map(|inferred| inferred.binding(reference.start_line))
    } else {
        None
    };
    if edge_type == EdgeType::Calls
        && let Some(ref receiver) = reference.receiver
        && (file_env.is_some() || inferred_receiver.is_some())
    {
        // A direct `this.field` receiver can use that owning class's exact
        // declared property type. A longer chain or unknown factory supplies
        // no result-type evidence.
        let field_binding = if matches!(language, Language::JavaScript | Language::TypeScript) {
            receiver
                .strip_prefix("this.")
                .filter(|field| {
                    !field.is_empty()
                        && field
                            .chars()
                            .all(|ch| ch.is_alphanumeric() || ch == '_' || ch == '$')
                })
                .and_then(|field| {
                    let owner = source_sym.parent_name.as_deref()?;
                    let properties: Vec<_> = symbol_map
                        .get(field)?
                        .iter()
                        .filter(|(file, symbol)| {
                            *file == file_path
                                && symbol.kind == SymbolKind::Property
                                && symbol.parent_name.as_deref() == Some(owner)
                                && symbol
                                    .type_info
                                    .as_ref()
                                    .and_then(|info| info.declared_type.as_ref())
                                    .is_some()
                        })
                        .collect();
                    if properties.len() != 1 {
                        return None;
                    }
                    let symbol = properties[0].1;
                    Some(crate::type_extractors::TypeBinding {
                        type_name: symbol.type_info.as_ref()?.declared_type.clone()?,
                        line: symbol.start_line,
                        confidence: 0.95,
                        source: crate::type_extractors::BindingSource::Annotation,
                    })
                })
        } else {
            None
        };
        let receiver_type = if field_binding.is_some() {
            field_binding.as_ref()
        } else if inferred_receiver.is_some() {
            inferred_receiver.as_ref()
        } else if let Some(env) = file_env.filter(|_| is_self_receiver(receiver)) {
            env.lookup_self(reference.start_line)
        } else if let Some(env) = file_env {
            // A member or call chain has its own result type. The first
            // segment's binding cannot establish the complete receiver's type.
            if let Some(call_scope) = reference.scope {
                if let Some(declaration) =
                    active_lexical_binding(scope_index, reference, receiver, language)
                {
                    env.lookup_declaration(receiver, declaration.scope?, call_scope.position)
                } else if language == Language::Rust {
                    let item_lines: Vec<_> = symbol_map
                        .get(receiver)
                        .into_iter()
                        .flatten()
                        .filter(|(file, symbol)| {
                            *file == file_path && symbol.kind == SymbolKind::Constant
                        })
                        .map(|(_, symbol)| symbol.start_line)
                        .collect();
                    env.lookup_scoped_item(receiver, &item_lines, call_scope.position)
                } else {
                    None
                }
            } else {
                env.lookup(receiver, reference.start_line)
            }
        } else {
            None
        };

        if let Some(binding) = receiver_type.filter(|binding| {
            reference.scope.is_some()
                || !name_is_locally_bound(
                    local_bindings,
                    scope_index,
                    sorted_syms,
                    reference,
                    language,
                )
                || binding.line >= source_sym.start_line
        }) {
            let method_name = &reference.name;
            let scoped_origin_required = reference.scope.is_some()
                && matches!(
                    language,
                    Language::Rust | Language::JavaScript | Language::TypeScript | Language::Go
                );
            let parts = binding.type_name_parts();
            let origin_file = parts.origin_file.unwrap_or(file_path);
            let unqualified_type = parts.raw_type;
            let (unqualified_type, origin_position) = unqualified_type
                .split_once('@')
                .map_or((unqualified_type, None), |(name, position)| {
                    (name, position.parse::<usize>().ok())
                });
            let constructor_shadowed = binding.source
                == crate::type_extractors::BindingSource::Constructor
                && matches!(language, Language::JavaScript | Language::TypeScript)
                && active_lexical_binding(scope_index, reference, unqualified_type, language)
                    .is_some();
            let origin = if constructor_shadowed {
                None
            } else {
                typed_receiver_origin(
                    origin_file,
                    unqualified_type,
                    reference,
                    symbol_map,
                    graph,
                    language,
                    type_envs.and_then(|envs| {
                        let env = envs.get(origin_file)?;
                        origin_position
                            .and_then(|position| env.rust_type_origins.get(&position))
                            .filter(|origin| origin.type_name == unqualified_type)
                            .map(|origin| RustReceiverEvidence {
                                origin,
                                type_envs: envs,
                            })
                    }),
                    false,
                )
            };
            let direct: Vec<_> = symbol_map
                .get(method_name.as_str())
                .into_iter()
                .flatten()
                .filter(|(file, symbol)| {
                    typed_member_accessible(file_path, file, source_sym, symbol, language, graph)
                        && if let Some(origin) = &origin {
                            method_belongs_to_origin(
                                file, symbol, origin, graph, symbol_map, type_envs,
                            )
                        } else {
                            !scoped_origin_required
                                && symbol.parent_name.as_deref() == Some(unqualified_type)
                        }
                })
                .collect();

            // A typed receiver still needs one accessible target at its exact
            // local/imported origin; duplicate class names are not evidence.
            if direct.len() == 1
                && let Some((candidate_file, sym)) = direct.first().copied()
            {
                let target_uid = symbol_uid(repo_uid, candidate_file, &sym.name, sym.start_line);
                let confidence = binding.confidence.min(0.95);
                return Some(ResolvedEdge {
                    source_uid,
                    target_uid,
                    edge_type,
                    confidence,
                    link_type: None,
                    evidence: vec![EdgeEvidence {
                        kind: "type_aware".to_string(),
                        weight: confidence,
                        note: Some(format!("{} -> {}", receiver, unqualified_type)),
                    }],
                });
            }

            // MRO walk: check parent types via inheritance chain
            let resolved_type = origin
                .as_ref()
                .map_or(unqualified_type, |origin| origin.name.as_str());
            if (!scoped_origin_required || origin.is_some())
                && symbol_map.get(resolved_type).is_none_or(|symbols| {
                    symbols
                        .iter()
                        .filter(|(_, symbol)| {
                            matches!(symbol.kind, SymbolKind::Class | SymbolKind::Enum)
                        })
                        .count()
                        <= 1
                })
            {
                let mut current_types = vec![resolved_type.to_string()];
                let mut visited = std::collections::HashSet::new();
                visited.insert(resolved_type.to_string());
                let mut depth = 0u32;

                while depth < 5 && !current_types.is_empty() {
                    let mut next_types = Vec::new();
                    for t in &current_types {
                        if let Some(parents) = extends_map.get(t.as_str()) {
                            for parent in parents {
                                if visited.contains(parent) {
                                    continue; // cycle guard
                                }
                                visited.insert(parent.clone());

                                let parent_origin = typed_receiver_origin(
                                    origin
                                        .as_ref()
                                        .map_or(file_path, |origin| origin.file.as_str()),
                                    parent,
                                    reference,
                                    symbol_map,
                                    graph,
                                    language,
                                    None,
                                    true,
                                );
                                let inherited: Vec<_> = symbol_map
                                    .get(method_name.as_str())
                                    .into_iter()
                                    .flatten()
                                    .filter(|(file, symbol)| {
                                        typed_member_accessible(
                                            file_path, file, source_sym, symbol, language, graph,
                                        ) && if let Some(origin) = &parent_origin {
                                            method_belongs_to_origin(
                                                file, symbol, origin, graph, symbol_map, type_envs,
                                            )
                                        } else {
                                            !scoped_origin_required
                                                && symbol.parent_name.as_deref()
                                                    == Some(parent.as_str())
                                        }
                                    })
                                    .collect();
                                if inherited.len() == 1
                                    && let Some((cf, sym)) = inherited.first().copied()
                                {
                                    let target_uid =
                                        symbol_uid(repo_uid, cf, &sym.name, sym.start_line);
                                    let conf =
                                        binding.confidence * 0.95_f32.powi((depth + 1) as i32);
                                    return Some(ResolvedEdge {
                                        source_uid,
                                        target_uid,
                                        edge_type,
                                        confidence: conf,
                                        link_type: None,
                                        evidence: vec![EdgeEvidence {
                                            kind: "type_aware_mro".to_string(),
                                            weight: conf,
                                            note: Some(format!(
                                                "MRO depth {} via {}",
                                                depth + 1,
                                                parent
                                            )),
                                        }],
                                    });
                                }
                                next_types.push(parent.clone());
                            }
                        }
                    }
                    current_types = next_types;
                    depth += 1;
                }
            }
            if scoped_origin_required {
                return Some(unresolved_edge(source_uid, &reference.name, edge_type));
            }
            // Older unscoped environments retain the existing lower tiers.
        }
    }

    // A method called on a call/`new` expression no evidence could type: the
    // name-based tiers below cannot match it (the receiver names no file and
    // no type), so without this it was always dropped. See
    // `unique_expression_method` for the gates that keep `get`/`then` out.
    if edge_type == EdgeType::Calls
        && inferred_receiver.is_none()
        && let Some(receiver) = reference.receiver.as_deref()
        && is_expression_receiver(receiver)
        && let Some((target_file, target)) = unique_expression_method(
            &receiver_inference,
            returned_constructions,
            receiver,
            &reference.name,
            source_sym,
        )
    {
        let confidence = confidence_score(MatchType::SamePackageFallback, language);
        return Some(ResolvedEdge {
            source_uid,
            target_uid: symbol_uid(repo_uid, target_file, &target.name, target.start_line),
            edge_type,
            confidence,
            link_type: None,
            evidence: vec![EdgeEvidence {
                kind: "unique_method_name".to_string(),
                weight: confidence,
                note: Some(format!("{receiver} -> {}", reference.name)),
            }],
        });
    }

    if name_is_locally_bound(
        local_bindings,
        scope_index,
        sorted_syms,
        reference,
        language,
    ) {
        return Some(unresolved_edge(source_uid, &reference.name, edge_type));
    }

    let name = &reference.name;

    let exact_rust_call = if language == Language::Rust && reference.receiver.is_none() {
        type_envs
            .and_then(|envs| envs.get(file_path))
            .and_then(|env| {
                env.call_assignments.iter().find(|assignment| {
                    assignment.receiver.is_none()
                        && assignment.callee == reference.name
                        && reference
                            .scope
                            .is_some_and(|scope| scope.position == assignment.call_position)
                })
            })
    } else {
        None
    };
    if exact_rust_call.is_some_and(|assignment| {
        assignment.local_callee_exists && assignment.local_callee_line.is_none()
    }) {
        return Some(unresolved_edge(source_uid, &reference.name, edge_type));
    }

    let rust_path_import_positions = if language == Language::Rust {
        type_envs
            .and_then(|envs| envs.get(file_path))
            .and_then(|env| {
                reference
                    .scope
                    .and_then(|scope| env.rust_path_imports.get(&scope.position))
            })
    } else {
        None
    };

    // Aliased import (`use path::to::original as name;`): the binding records
    // the original name and the file it was imported from.
    let module_binding = rust_path_import_positions.and_then(|positions| {
        let receiver = reference.receiver.as_deref()?;
        graph.bindings_named(file_path, receiver).find(|binding| {
            binding.original_name == "*"
                && binding
                    .scope
                    .is_some_and(|scope| positions.contains(&scope.position))
        })
    });
    // Only a binding named like the call or its receiver can match below, so
    // look those up by name instead of scanning every import in the file.
    let binding = module_binding.or_else(|| {
        let candidates =
            graph.bindings_named_either(file_path, name, reference.receiver.as_deref());
        candidates.into_iter().find(|binding| {
            let in_scope =
                if let (Some(binding_scope), Some(call_scope)) = (binding.scope, reference.scope) {
                    binding_scope.start <= call_scope.position
                        && call_scope.position < binding_scope.end
                        && (binding_scope.initialized_at <= call_scope.position
                            // nw-113: a function body written above a
                            // `require` runs later, once the binding is
                            // initialized (the exemption `name_is_locally_bound`
                            // gives a nested body). Top-level code, even in a
                            // block or an IIFE, runs first: it has no source
                            // but the module owner.
                            || !source_sym.is_module_owner()
                                && call_scope.start > binding_scope.start
                                && call_scope.end <= binding_scope.end)
                } else {
                    scoping_owner(sorted_syms, binding.start_line, language).is_none_or(|owner| {
                        owner.start_line == source_sym.start_line && owner.name == source_sym.name
                    })
                };
            in_scope
                && rust_path_import_positions.is_none_or(|positions| {
                    language != Language::Rust
                        || binding.original_name == "*"
                        || binding
                            .scope
                            .is_some_and(|scope| positions.contains(&scope.position))
                })
                && exact_rust_call.is_none_or(|assignment| {
                    assignment.local_callee_line.is_none()
                        && binding.scope.is_some_and(|scope| {
                            assignment.import_positions.contains(&scope.position)
                        })
                })
                && if binding.original_name == "*" {
                    reference.receiver.as_deref() == Some(binding.local_name.as_str())
                        // nw-801: a CommonJS module bound whole (`import x =
                        // require('…')`, `const x = require('…')`) can itself
                        // be called. A bare `x()` names that import, never a
                        // same-named declaration elsewhere in the file.
                        || matches!(language, Language::JavaScript | Language::TypeScript)
                            && reference.receiver.is_none()
                            && binding.local_name == *name
                } else {
                    (binding.local_name == *name && reference.receiver.is_none())
                        || reference.receiver.as_deref() == Some(binding.local_name.as_str())
                }
        })
    });
    // nw-113: an unresolved import binding still shadows every same-file
    // declaration except a nested one: a strictly narrower visible
    // declarator (both scopes contain the call, so the widths nest), or a
    // function declaration whose innermost enclosing function or method also
    // contains the call (see `nested_inside_binding`). Single-file passes
    // (watcher, incremental index) see an import of a sibling file as
    // unresolved because the sibling is not in the resolution set, so without
    // that exception they dropped the nested edge the full index keeps.
    let unresolved_binding = binding.filter(|binding| binding.source_file.is_none());

    // Priority 1: Same file. A local symbol shadows an import alias of the
    // same name, so this check runs on the reference's own name, before any
    // alias rewriting. nw-113: a block-scoped declaration shadows only inside
    // its block, and the innermost visible declaration wins. A receiver that
    // names an import (`Modal.latestProps` after `import { Modal }`) denotes
    // that import's member, never a same-file symbol of the bare member name,
    // even when the file stem matches the receiver (`Modal.test.js`).
    let mut same_file_out_of_scope = false;
    let mut rejected_member = false;
    if let Some(syms) = symbol_map.get(name.as_str())
        && let Some((_, sym)) = syms
            .iter()
            .filter(|(f, sym)| {
                *f == file_path
                    && !(reference.receiver.is_some() && binding.is_some())
                    && exact_rust_call.is_none_or(|assignment| {
                        assignment
                            .local_callee_line
                            .is_some_and(|line| sym.start_line == line)
                    })
                    && candidate_matches(reference, file_path, f, sym, language, graph)
                    && {
                        // nw-789: a bare call never names a same-file method,
                        // but the method's presence is still this file's
                        // declaration of the name: Priority 4 must not bind a
                        // sibling file's homonym in its place.
                        let member = bare_call_names_member(reference, sym, language);
                        rejected_member |= member;
                        !member
                    }
            })
            // Records rejections as a side effect; `min_by_key` consumes the
            // whole iterator, so every candidate is seen. An unresolved import
            // admits only a nested declaration. That test runs per candidate,
            // before the innermost is chosen, so a rejected homonym (a sibling
            // function's nested `f`) cannot hide an admissible one.
            .filter_map(|candidate| {
                let width = same_file_declaration_scope(
                    scope_index,
                    sorted_syms,
                    candidate.1,
                    reference,
                    language,
                );
                same_file_out_of_scope |= width.is_none();
                let width = width?;
                unresolved_binding
                    .is_none_or(|binding| {
                        // A Python width is a line span, not comparable with
                        // a byte scope: a Python candidate is admitted by
                        // explicit containment alone.
                        language != Language::Python
                            && binding
                                .scope
                                .is_some_and(|scope| width < scope.end - scope.start)
                            || nested_inside_binding(
                                scope_index,
                                sorted_syms,
                                candidate.1,
                                reference,
                                binding,
                                language,
                            )
                    })
                    .then_some((width, candidate))
            })
            // The innermost scope wins; a tie goes to the earliest declaration,
            // so the choice never depends on `symbol_map` order.
            .min_by_key(|(width, (_, sym))| (*width, sym.start_line))
            .map(|(_, candidate)| candidate)
    {
        let target_uid = symbol_uid(repo_uid, file_path, &sym.name, sym.start_line);
        let confidence = confidence_score(MatchType::SameFileExact, language);
        return Some(ResolvedEdge {
            source_uid,
            target_uid,
            edge_type,
            confidence,
            link_type: None,
            evidence: vec![EdgeEvidence {
                kind: "same_file".to_string(),
                weight: confidence,
                note: None,
            }],
        });
    }

    same_file_out_of_scope |= rejected_member;

    // Otherwise an import whose source is not in this resolution set binds
    // nothing: no sibling homonym or later tier may stand in for it.
    if unresolved_binding.is_some() {
        return Some(unresolved_edge(source_uid, &reference.name, edge_type));
    }

    if exact_rust_call.is_some_and(|assignment| {
        assignment.local_callee_line.is_none()
            && (assignment.import_paths.is_empty()
                || !assignment.import_positions.is_empty() && binding.is_none())
    }) {
        return Some(unresolved_edge(source_uid, name, edge_type));
    }

    if let Some(binding) = binding {
        // A Rust module namespace is supported by an AST qualified call,
        // never by a value receiver with the same spelling.
        if language == Language::Rust
            && binding.original_name == "*"
            && type_envs.is_some_and(|envs| envs.contains_key(file_path))
            && !rust_path_import_positions.is_some_and(|positions| {
                binding
                    .scope
                    .is_some_and(|scope| positions.contains(&scope.position))
            })
        {
            return Some(unresolved_edge(source_uid, name, edge_type));
        }
        // nw-801: calling a whole-module binding calls what the module
        // exports as its value, which no export name identifies.
        if binding.original_name == "*"
            && reference.receiver.is_none()
            && matches!(language, Language::JavaScript | Language::TypeScript)
        {
            return Some(unresolved_edge(source_uid, name, edge_type));
        }
        let original = if binding.original_name == "*" {
            name.as_str()
        } else {
            binding.original_name.as_str()
        };
        // An unresolved binding returned above; this only keeps the
        // invariant local.
        let Some(source_file) = binding.source_file.as_deref() else {
            return Some(unresolved_edge(source_uid, name, edge_type));
        };
        let exact_exports = matches!(language, Language::JavaScript | Language::TypeScript);
        // A Rust type path resolves through the same bounded named type route
        // as a typed value. Keep the terminal declaration's byte identity so
        // inline types and re-exports cannot donate a same-named hidden impl.
        let mut rust_parent_member_origin = false;
        let rust_member_origin = if language == Language::Rust
            && binding.original_name != "*"
            && reference.receiver.is_some()
            && rust_path_import_positions.is_some()
        {
            type_envs.and_then(|envs| {
                let direct_parent = rust_direct_parent_type_origin(
                    file_path,
                    source_file,
                    original,
                    binding.rust_inline_modules.as_deref()?,
                    symbol_map,
                    graph,
                    envs,
                );
                rust_parent_member_origin = direct_parent.is_some();
                direct_parent.or_else(|| {
                    rust_public_type_origin(
                        source_file,
                        original,
                        binding.rust_inline_modules.as_deref()?,
                        symbol_map,
                        graph,
                        envs,
                    )
                })
            })
        } else {
            None
        };
        let (target_file, original, depth, literal_method_line) = if let Some(origin) =
            &rust_member_origin
        {
            (origin.file.as_str(), origin.name.as_str(), 0, None)
        } else if exact_exports {
            let Some(target) = graph.exported_target_with_declaration(source_file, original) else {
                return Some(unresolved_edge(source_uid, name, edge_type));
            };
            target
        } else {
            (source_file, original, 0, None)
        };
        let member_parent = if binding.original_name != "*" && reference.receiver.is_some() {
            Some(original)
        } else {
            None
        };
        let receiver_declarations: Vec<_> = symbol_map
            .get(original)
            .into_iter()
            .flatten()
            .filter(|(file, symbol)| *file == target_file && symbol.parent_name.is_none())
            .collect();
        let member_receiver = if let Some(origin) = &rust_member_origin {
            Some((origin.name.as_str(), true))
        } else if member_parent.is_some() && receiver_declarations.len() == 1 {
            let symbol = receiver_declarations[0].1;
            if symbol.kind == SymbolKind::Class {
                Some((original, true))
            } else {
                symbol
                    .type_info
                    .as_ref()
                    .and_then(|info| info.declared_type.as_deref())
                    .and_then(|receiver_type| {
                        if receiver_type == "object" {
                            Some((original, false))
                        } else if symbol_map.get(receiver_type).is_some_and(|types| {
                            types
                                .iter()
                                .filter(|(file, symbol)| {
                                    *file == target_file && symbol.kind == SymbolKind::Class
                                })
                                .count()
                                == 1
                        }) {
                            Some((receiver_type, false))
                        } else {
                            None
                        }
                    })
            }
        } else {
            None
        };
        if member_parent.is_some() && member_receiver.is_none() {
            return Some(unresolved_edge(source_uid, name, edge_type));
        }
        let target_name = member_parent.map_or(original, |_| name.as_str());
        if let Some(symbols) = symbol_map.get(target_name) {
            let explicit_private = language == Language::Python
                || language == Language::Rust
                    && (graph.resolves_parent(file_path, target_file)
                        || rust_parent_member_origin && file_path == target_file);
            let targets: Vec<_> = symbols
                .iter()
                .filter(|(file, symbol)| {
                    *file == target_file
                        && (language != Language::Rust
                            || binding.original_name != "*"
                            || symbol.parent_name.is_none() && symbol.scope_chain.is_none())
                        && if let Some(line) = literal_method_line {
                            symbol.start_line == line
                                && symbol.kind == SymbolKind::Method
                                && symbol.parent_name.as_deref() == Some("module.exports")
                        } else {
                            !exact_exports
                                || member_receiver.is_some()
                                || symbol.parent_name.is_none()
                        }
                        && (symbol.visibility != Visibility::Private
                            || explicit_private
                            || member_receiver.is_some_and(|(_, class)| !class))
                        && member_receiver.is_none_or(|(parent, class)| {
                            let prefix = symbol.signature.split('(').next().unwrap_or("");
                            symbol.kind == SymbolKind::Method
                                && symbol.parent_name.as_deref() == Some(parent)
                                && !prefix.split_whitespace().rev().skip(1).any(|token| {
                                    matches!(token, "private" | "protected" | "get" | "set")
                                })
                                && !symbol.name.starts_with('#')
                                // Python class attributes expose functions,
                                // staticmethods and classmethods alike. The
                                // exact imported class supplies receiver evidence.
                                && (language == Language::Python
                                    // Rust has no `static` function modifier.
                                    // A qualified exact type also permits
                                    // explicit-self UFCS: Type::method(&value).
                                    || language == Language::Rust
                                        && class
                                        && rust_member_origin.as_ref().is_some_and(|origin| {
                                            method_belongs_to_origin(
                                                file, symbol, origin, graph,
                                                symbol_map, type_envs,
                                            )
                                        })
                                    || prefix
                                        .split_whitespace()
                                        .rev()
                                        .skip(1)
                                        .any(|token| token == "static")
                                        == class)
                        })
                        && candidate_matches(
                            &RawReference {
                                name: target_name.into(),
                                receiver: None,
                                ..reference.clone()
                            },
                            if explicit_private || member_receiver.is_some_and(|(_, class)| !class)
                            {
                                target_file
                            } else {
                                file_path
                            },
                            file,
                            symbol,
                            language,
                            graph,
                        )
                })
                .collect();
            let selected = if targets.len() == 1 {
                targets.first().copied()
            } else if language == Language::TypeScript
                && targets.iter().all(|(_, symbol)| {
                    symbol.kind == SymbolKind::Function && symbol.parent_name.is_none()
                })
            {
                let implementations: Vec<_> = targets
                    .iter()
                    .copied()
                    .filter(|(_, symbol)| {
                        symbol.kind == SymbolKind::Function
                            && !symbol.signature.trim_end().ends_with(';')
                    })
                    .collect();
                if implementations.len() == 1 {
                    implementations.first().copied()
                } else {
                    None
                }
            } else {
                None
            };
            if let Some((file, symbol)) = selected {
                if targets.len() > 1 && language != Language::TypeScript {
                    return Some(unresolved_edge(source_uid, name, edge_type));
                }
                {
                    let confidence = confidence_score(
                        if depth == 0 {
                            MatchType::ImportResolved
                        } else {
                            MatchType::ReExportResolved
                        },
                        language,
                    );
                    return Some(ResolvedEdge {
                        source_uid,
                        target_uid: symbol_uid(repo_uid, file, &symbol.name, symbol.start_line),
                        edge_type,
                        confidence,
                        link_type: None,
                        evidence: vec![EdgeEvidence {
                            kind: if depth == 0 {
                                "import_alias"
                            } else {
                                "reexport_resolved"
                            }
                            .into(),
                            weight: confidence,
                            note: Some(format!("{} -> {}", binding.local_name, original)),
                        }],
                    });
                }
            }
            if symbols.iter().any(|(file, symbol)| {
                *file == target_file && symbol.visibility == Visibility::Private
            }) {
                return Some(unresolved_edge(source_uid, name, edge_type));
            }
        }
        if exact_exports {
            return Some(unresolved_edge(source_uid, name, edge_type));
        }
    }

    if matches!(language, Language::JavaScript | Language::TypeScript)
        && !graph.imports_of(file_path).is_empty()
        && binding.is_none()
    {
        return Some(unresolved_edge(source_uid, name, edge_type));
    }

    // Fall back to resolving the original name through the normal priority
    // chain (e.g. the aliased item is re-exported from another import).
    let effective_name = binding.map_or_else(|| name.clone(), |b| b.original_name.clone());

    let candidates = symbol_map.get(effective_name.as_str());

    // Priority 1.5: explicit path qualifier (nw-152).
    //
    // The .scm captures only the trailing identifier of a scoped call, so
    // `nestweaver_engine::publication::read_current(..)` arrived here as the
    // bare name `read_current`. With no `use` for that module in the file, it
    // matched nothing in the tiers below and fell through to
    // `unresolved:read_current` at confidence 0.0 -- the edge was dropped
    // entirely. Resolution accuracy therefore depended on which UNRELATED types
    // a file happened to import.
    //
    // The parser now records the qualifier as the reference receiver, so prefer
    // a candidate whose file stem matches the qualifier's last module segment.
    //
    // Gated on a path receiver (`module::Type`), not on a value expression
    // that merely mentions `::`. A bare receiver -- a JS variable in
    // `store.method()`, or the type in `HashMap::new()` -- is excluded,
    // because matching those against a same-named file would invent edges
    // rather than recover them.
    if let Some(qualifier) = reference.receiver.as_deref()
        && is_path_receiver(qualifier)
        && let Some(syms) = &candidates
        && let Some(module) = qualifier.rsplit("::").find(|segment| !segment.is_empty())
    {
        let mut qualified: Vec<_> = syms
            .iter()
            .filter(|(candidate_file, sym)| {
                candidate_matches(reference, file_path, candidate_file, sym, language, graph)
                    && candidate_file
                        .rsplit('/')
                        .next()
                        .and_then(|base| base.split('.').next())
                        .is_some_and(|stem| stem == module)
            })
            .collect();
        qualified.sort_by_key(|(path, _)| *path);
        if let Some((candidate_file, sym)) = qualified.into_iter().next() {
            let target_uid = symbol_uid(repo_uid, candidate_file, &sym.name, sym.start_line);
            let confidence = confidence_score(MatchType::ImportResolved, language);
            return Some(ResolvedEdge {
                source_uid,
                target_uid,
                edge_type,
                confidence,
                link_type: None,
                evidence: vec![EdgeEvidence {
                    kind: "path_qualified".to_string(),
                    weight: confidence,
                    note: Some(format!("{qualifier}::{name}")),
                }],
            });
        }
    }

    // nw-308 / nw-327: the receiver gate. See `candidate_matches` -- the
    // nw-150 fix put exactly this test in, but only on Priority 4, the
    // WEAKEST tier. Priorities 2 and 3 return first and were ungated, so
    // importing ANY symbol from a file donated every bare method name in it:
    // `.collect()` in `tools.rs` bound to a private `SegmentCollector::collect`
    // in `tantivy_index.rs` purely because `tools.rs:30` imports `SearchTotal`
    // from that file. The comment at the Priority 4 tier below is a verbatim
    // description of this bug at a different tier. nw-724 extends the same
    // gate to the same-file tier and refuses calls that land on a field,
    // type alias, or macro invocation.

    // Priority 2: Direct imports
    let mut imports = if let Some(binding) = binding {
        vec![(
            binding.local_name.clone(),
            binding.source_file.clone().expect("resolved binding"),
        )]
    } else {
        graph
            .imports_of(file_path)
            .into_iter()
            .filter(|(specifier, _)| {
                exact_rust_call.is_none_or(|assignment| assignment.import_paths.contains(specifier))
            })
            .collect()
    };
    imports.sort_by(|(_, a), (_, b)| a.cmp(b));
    // Kotlin/Java: a single-type import names one top-level declaration (not
    // its whole file) and shadows a same-package declaration, which shadows
    // an on-demand import (JLS 6.4.1; Kotlin: explicit > same package >
    // star). `jvm_shadowed` hands an on-demand-only name to the same-package
    // tier below.
    let jvm_imports = binding
        .is_none()
        .then(|| graph.jvm_imports_of(file_path))
        .flatten()
        .filter(|_| matches!(language, Language::Kotlin | Language::Java));
    let jvm_shadowed = jvm_imports.is_some()
        && candidates.is_some_and(|syms| {
            syms.iter().any(|(f, sym)| {
                // A bare name denotes a package member: a top-level one.
                *f != file_path
                    && (reference.receiver.is_some() || sym.parent_name.is_none())
                    && graph.jvm_same_package(file_path, f)
                    && candidate_matches(reference, file_path, f, sym, language, graph)
                    && !bare_call_names_member(reference, sym, language)
            })
        });
    let mut imported: Option<(Option<JvmImportRank>, &str, &RawSymbol)> = None;
    for (_, imported_file) in &imports {
        let Some(syms) = &candidates else {
            break;
        };
        let found = syms.iter().find_map(|(f, sym)| {
            if f != imported_file {
                return None;
            }
            let rank = match jvm_imports {
                None => None,
                Some(jvm) => match jvm.rank(
                    f,
                    sym.start_line,
                    reference.receiver.is_none().then_some(sym.name.as_str()),
                )? {
                    JvmImportRank::OnDemand if jvm_shadowed => return None,
                    rank => Some(rank),
                },
            };
            candidate_matches(
                reference,
                if language == Language::Rust && graph.resolves_parent(file_path, f) {
                    f
                } else {
                    file_path
                },
                f,
                sym,
                language,
                graph,
            )
            .then_some((rank, *f, *sym))
        });
        if let Some(found) = found
            && imported.is_none_or(|(rank, _, _)| found.0 < rank)
        {
            imported = Some(found);
            if found
                .0
                .is_none_or(|rank| rank == JvmImportRank::Declaration)
            {
                break;
            }
        }
    }
    if let Some((_, imported_file, sym)) = imported {
        let target_uid = symbol_uid(repo_uid, imported_file, &sym.name, sym.start_line);
        let confidence = confidence_score(MatchType::ImportResolved, language);
        return Some(ResolvedEdge {
            source_uid,
            target_uid,
            edge_type,
            confidence,
            link_type: None,
            evidence: vec![EdgeEvidence {
                kind: "import_resolved".to_string(),
                weight: confidence,
                note: None,
            }],
        });
    }

    // Priority 3: Re-exports.
    //
    // nw-323 (defect C, second half): this walked exactly ONE hop, but a real
    // TypeScript barrel chain is two or more -- `common/errors.ts ->
    // errors/index.ts -> http-errors.ts` is the chain `NotFoundError` needs.
    // Bounded at REEXPORT_MAX_HOPS with a visited set so a cyclic barrel (which
    // TypeScript permits) cannot loop, and so the fan-out stays bounded: this
    // tier is why nw-153's guards exist.
    // Kotlin/Java imports re-export nothing; a same-package name shadowed
    // its on-demand imports and goes to the same-package tier.
    let mut frontier: Vec<String> = if jvm_shadowed {
        Vec::new()
    } else {
        imports.iter().map(|(_, f)| f.to_string()).collect()
    };
    let mut visited: std::collections::HashSet<String> = frontier.iter().cloned().collect();
    visited.insert(file_path.to_string());
    for _ in 0..REEXPORT_MAX_HOPS {
        let mut next_frontier: Vec<String> = Vec::new();
        for imported_file in &frontier {
            let mut transitive_imports = graph.imports_of(imported_file);
            transitive_imports.sort_by(|(_, a), (_, b)| a.cmp(b));
            for (_, transitive_file) in &transitive_imports {
                if !visited.insert(transitive_file.to_string()) {
                    continue;
                }
                next_frontier.push(transitive_file.to_string());
            }
        }
        next_frontier.sort();
        for transitive_file in &next_frontier {
            if let Some(syms) = &candidates
                && let Some((_, sym)) = syms.iter().find(|(f, sym)| {
                    *f == transitive_file.as_str()
                        && candidate_matches(reference, file_path, f, sym, language, graph)
                })
            {
                let target_uid = symbol_uid(repo_uid, transitive_file, &sym.name, sym.start_line);
                let confidence = confidence_score(MatchType::ReExportResolved, language);
                return Some(ResolvedEdge {
                    source_uid,
                    target_uid,
                    edge_type,
                    confidence,
                    link_type: None,
                    evidence: vec![EdgeEvidence {
                        kind: "reexport_resolved".to_string(),
                        weight: confidence,
                        note: None,
                    }],
                });
            }
        }
        if next_frontier.is_empty() {
            break;
        }
        frontier = next_frontier;
    }

    if binding.is_some() {
        return Some(unresolved_edge(source_uid, name, edge_type));
    }

    // nw-113: the name is declared in this file, but Priority 1 rejected
    // every such candidate: out of scope at this call (a block-scoped JS/TS
    // declaration, a Python `def` nested in another function), or (nw-789) a
    // method a bare JS/TS call cannot name. The file's own declaration is the
    // one the name denotes, so the same-directory fallback below would
    // fabricate the edge. A JS/TS file reaches here only when it has no
    // imports (one with imports returned unresolved above for an unbound
    // name); a Python file reaches here with or without imports. A name not
    // declared in this file at all still takes the same-directory fallback.
    if same_file_out_of_scope {
        return Some(unresolved_edge(source_uid, name, edge_type));
    }

    // Priority 4: Same package/directory
    //
    // nw-150: for a METHOD call this fallback invents edges. `knex.where(..)`
    // is captured as a call to the bare name `where`, and binding that to
    // whatever same-named symbol happens to sit in a sibling file made a
    // block-scoped `const where = {..}` the single most-depended-on symbol in a
    // 193k-symbol graph (in_degree 1048), with 524 CALLS "dependents" that were
    // Knex query-builder calls in files that never import it. It poisoned hubs,
    // bridges, PageRank and repo-map alike.
    //
    // A value receiver is only evidence for a target if it plausibly denotes
    // it, so require the candidate's file stem to match the receiver. A path
    // receiver (containing `::`) is already handled by the qualified tier
    // above, and a receiver-less plain call keeps the original behaviour.
    let same_dir = parent_dir(file_path);
    if let Some(syms) = &candidates {
        let mut same_pkg: Vec<_> = syms
            .iter()
            .filter(|(candidate_file, _)| {
                *candidate_file != file_path
                    && if matches!(language, Language::Kotlin | Language::Java) {
                        graph.jvm_same_package(file_path, candidate_file)
                    } else {
                        parent_dir(candidate_file) == same_dir
                            // A `//go:build ignore` program shares no package.
                            && !(language == Language::Go
                                && (graph.go_build_ignored(file_path)
                                    || graph.go_build_ignored(candidate_file)))
                    }
            })
            .filter(|(candidate_file, sym)| {
                candidate_matches(reference, file_path, candidate_file, sym, language, graph)
                    && !bare_call_names_member(reference, sym, language)
            })
            .collect();
        same_pkg.sort_by_key(|(path, _)| *path);
        if let Some((candidate_file, sym)) = same_pkg.into_iter().next() {
            let target_uid = symbol_uid(repo_uid, candidate_file, &sym.name, sym.start_line);
            let confidence = confidence_score(MatchType::SamePackageFallback, language);
            return Some(ResolvedEdge {
                source_uid,
                target_uid,
                edge_type,
                confidence,
                link_type: None,
                evidence: vec![EdgeEvidence {
                    kind: "same_package".to_string(),
                    weight: confidence,
                    note: None,
                }],
            });
        }
    }

    // No match → unresolved. A path receiver that denoted nothing is included:
    // `candidate_matches` does not treat it as a bare name, which is how
    // `Vec::len` used to bind whatever `fn len` an import happened to donate.
    Some(unresolved_edge(source_uid, name, edge_type))
}

/// Whether `reference` may name `sym`.
///
/// nw-724. A call is not a field access and a macro invocation is not a
/// call: `format!(...)` was binding to a struct field named `format`, and
/// `Err(...)` to `type Err = String`. A method call (`items.len()`) does not
/// name a free function in a file the caller happens to import. A field
/// access may still name a field in the same file; it may not name a function.
#[derive(Clone, PartialEq, Eq)]
struct TypedReceiverOrigin {
    file: String,
    name: String,
    rust_class_position: Option<usize>,
}

struct RustReceiverEvidence<'a> {
    origin: &'a nestweaver_parser::parse::ScopedRustTypeOrigin,
    type_envs: &'a std::collections::HashMap<String, TypeEnvironment>,
}

fn rust_module_type_position(
    file: &str,
    name: &str,
    modules: &[String],
    line: u32,
    type_envs: &std::collections::HashMap<String, TypeEnvironment>,
) -> Option<usize> {
    let declarations = type_envs
        .get(file)?
        .rust_module_types
        .get(&(name.into(), modules.to_vec()))?;
    let mut positions = declarations
        .iter()
        .filter(|declaration| declaration.line == line)
        .map(|declaration| declaration.position);
    let position = positions.next()?;
    positions.next().is_none().then_some(position)
}

/// A direct root parent type and its methods are visible to child modules.
/// This never traverses a barrel or admits another inline module's private type.
fn rust_direct_parent_type_origin(
    source_file: &str,
    file: &str,
    name: &str,
    modules: &[String],
    symbol_map: &std::collections::HashMap<String, Vec<(&str, &RawSymbol)>>,
    graph: &ImportGraph,
    type_envs: &std::collections::HashMap<String, TypeEnvironment>,
) -> Option<TypedReceiverOrigin> {
    if !modules.is_empty() || !(source_file == file || graph.resolves_parent(source_file, file)) {
        return None;
    }
    let declarations: Vec<_> = symbol_map
        .get(name)
        .into_iter()
        .flatten()
        .filter(|(owner_file, symbol)| {
            *owner_file == file
                && matches!(symbol.kind, SymbolKind::Class | SymbolKind::Enum)
                && symbol.parent_name.is_none()
                && symbol.scope_chain.is_none()
                && rust_module_type_position(file, name, &[], symbol.start_line, type_envs)
                    .is_some()
        })
        .collect();
    if declarations.len() != 1 {
        return None;
    }
    let position =
        rust_module_type_position(file, name, &[], declarations[0].1.start_line, type_envs)?;
    Some(TypedReceiverOrigin {
        file: file.into(),
        name: name.into(),
        rust_class_position: Some(position),
    })
}

/// Follow a unique public named type route, never a file's unrelated imports.
fn rust_public_type_origin(
    file: &str,
    name: &str,
    modules: &[String],
    symbol_map: &std::collections::HashMap<String, Vec<(&str, &RawSymbol)>>,
    graph: &ImportGraph,
    type_envs: &std::collections::HashMap<String, TypeEnvironment>,
) -> Option<TypedReceiverOrigin> {
    let mut file = file.to_string();
    let mut name = name.to_string();
    let mut modules = modules.to_vec();
    let mut visited = std::collections::HashSet::new();
    for depth in 0..=3 {
        if !visited.insert((file.clone(), name.clone(), modules.clone())) {
            return None;
        }
        let module_path = modules.join("::");
        let declarations: Vec<_> = symbol_map
            .get(&name)
            .into_iter()
            .flatten()
            .filter(|(owner, symbol)| {
                *owner == file
                    && matches!(symbol.kind, SymbolKind::Class | SymbolKind::Enum)
                    && symbol.parent_name.is_none()
                    && symbol.scope_chain.as_deref()
                        == if modules.is_empty() {
                            None
                        } else {
                            Some(module_path.as_str())
                        }
                    && rust_module_type_position(
                        &file,
                        &name,
                        &modules,
                        symbol.start_line,
                        type_envs,
                    )
                    .is_some()
            })
            .collect();
        if !declarations.is_empty() {
            if declarations.len() != 1 || declarations[0].1.visibility == Visibility::Private {
                return None;
            }
            let position = rust_module_type_position(
                &file,
                &name,
                &modules,
                declarations[0].1.start_line,
                type_envs,
            )?;
            return Some(TypedReceiverOrigin {
                file,
                name,
                rust_class_position: Some(position),
            });
        }
        if depth == 3 || !modules.is_empty() {
            return None;
        }
        let public_uses = &type_envs.get(&file)?.rust_public_root_uses;
        let bindings: Vec<_> = graph
            .bindings_named(&file, &name)
            .filter(|binding| {
                binding.original_name != "*"
                    && binding
                        .scope
                        .is_some_and(|scope| public_uses.contains(&scope.position))
            })
            .collect();
        if bindings.len() != 1 {
            return None;
        }
        let binding = bindings[0];
        modules = binding.rust_inline_modules.as_ref()?.clone();
        file = binding.source_file.as_ref()?.clone();
        name = binding.original_name.clone();
    }
    None
}

#[allow(clippy::too_many_arguments)]
fn typed_receiver_origin(
    source_file: &str,
    type_name: &str,
    reference: &RawReference,
    symbol_map: &std::collections::HashMap<String, Vec<(&str, &RawSymbol)>>,
    graph: &ImportGraph,
    language: Language,
    rust_origin: Option<RustReceiverEvidence<'_>>,
    admit_traits: bool,
) -> Option<TypedReceiverOrigin> {
    // An inherited lookup (the MRO walk) may land on a trait or interface:
    // a Rust trait's default method, a Java default method. A receiver's own
    // declared type still has to be a class or enum.
    let type_kind = |kind: SymbolKind| {
        matches!(kind, SymbolKind::Class | SymbolKind::Enum)
            || admit_traits && matches!(kind, SymbolKind::Trait | SymbolKind::Interface)
    };
    if language == Language::Rust
        && let Some(RustReceiverEvidence { origin, type_envs }) = rust_origin
    {
        if origin.ambiguous {
            return None;
        }
        if let Some(line) = origin.local_line {
            return symbol_map
                .get(type_name)?
                .iter()
                .find(|(file, symbol)| {
                    *file == source_file
                        && symbol.start_line == line
                        && matches!(symbol.kind, SymbolKind::Class | SymbolKind::Enum)
                })
                .map(|_| TypedReceiverOrigin {
                    file: source_file.into(),
                    name: type_name.into(),
                    rust_class_position: origin.local_position,
                });
        }
        let imports = graph.imports_of(source_file);
        if origin
            .named_imports
            .iter()
            .any(|path| !imports.iter().any(|(specifier, _)| specifier == path))
        {
            return None;
        }
        let selected_paths = if origin.named_imports.is_empty() {
            &origin.imports
        } else {
            &origin.named_imports
        };
        // Named aliases retain their original declaration spelling. Unresolved
        // exact imports cannot be rescued by another resolved glob.
        let named: Vec<_> = graph
            .bindings_named(source_file, type_name)
            .filter(|binding| {
                binding
                    .scope
                    .is_some_and(|scope| origin.named_import_positions.contains(&scope.position))
                    && imports.iter().any(|(specifier, file)| {
                        selected_paths.contains(specifier)
                            && binding.source_file.as_deref() == Some(file.as_str())
                    })
            })
            .collect();
        if !origin.named_imports.is_empty() && named.len() != 1 {
            return None;
        }
        let mut candidates = Vec::new();
        let mut seen_files = std::collections::HashSet::new();
        for (_specifier, file) in imports.iter().filter(|(specifier, file)| {
            selected_paths.contains(specifier)
                && (named.is_empty()
                    || named
                        .iter()
                        .any(|binding| binding.source_file.as_deref() == Some(file.as_str())))
        }) {
            if !seen_files.insert(file) {
                continue;
            }
            let name = named
                .iter()
                .find(|binding| binding.source_file.as_deref() == Some(file.as_str()))
                .map_or(type_name, |binding| binding.original_name.as_str());
            let modules = named
                .iter()
                .find(|binding| binding.source_file.as_deref() == Some(file.as_str()))
                .map(|binding| binding.rust_inline_modules.as_deref());
            let Some(modules) = modules.unwrap_or(Some(&[])) else {
                continue;
            };
            if let Some(candidate) =
                rust_public_type_origin(file, name, modules, symbol_map, graph, type_envs)
            {
                candidates.push(candidate);
            } else if (source_file != file || !named.is_empty())
                && let Some(candidate) = rust_direct_parent_type_origin(
                    source_file,
                    file,
                    name,
                    modules,
                    symbol_map,
                    graph,
                    type_envs,
                )
            {
                // Preserve the existing cross-file parent route. Newly admitted
                // same-file routes must carry an exact named import declaration.
                candidates.push(candidate);
            }
        }
        return (candidates.len() == 1).then(|| candidates.remove(0));
    }
    if language == Language::Go {
        let (qualifier, name) = type_name
            .split_once('.')
            .map_or((None, type_name), |(package, name)| (Some(package), name));
        let qualified =
            qualifier.and_then(|package| graph.go_qualified_package(source_file, package));
        let candidates: Vec<_> = symbol_map
            .get(name)
            .into_iter()
            .flatten()
            .filter(|(file, symbol)| {
                type_kind(symbol.kind)
                    && match qualifier {
                        Some(_) => {
                            qualified.is_some_and(|files| files.iter().any(|target| target == file))
                                && symbol.visibility != Visibility::Private
                        }
                        None => {
                            parent_dir(file) == parent_dir(source_file)
                                && go_same_package(graph, source_file, file)
                        }
                    }
            })
            .collect();
        return (candidates.len() == 1).then(|| TypedReceiverOrigin {
            file: candidates[0].0.into(),
            name: name.into(),
            rust_class_position: None,
        });
    }
    let local: Vec<_> = symbol_map
        .get(type_name)
        .into_iter()
        .flatten()
        .filter(|(file, symbol)| *file == source_file && type_kind(symbol.kind))
        .collect();
    if local.len() == 1 {
        return Some(TypedReceiverOrigin {
            file: source_file.into(),
            name: type_name.into(),
            rust_class_position: None,
        });
    }
    if !local.is_empty() {
        return None;
    }
    let imports: Vec<_> = graph
        .bindings_named(source_file, type_name)
        .filter(|binding| {
            binding.original_name != "*"
                && binding.scope.is_none_or(|scope| {
                    reference.scope.is_some_and(|call| {
                        scope.start <= call.position
                            && call.position < scope.end
                            && scope.initialized_at <= call.position
                    })
                })
        })
        .collect();
    if imports.len() == 1 {
        let binding = imports[0];
        let file = binding.source_file.as_deref()?;
        let (file, name, _) = if matches!(language, Language::JavaScript | Language::TypeScript) {
            graph.exported_target(file, &binding.original_name)?
        } else {
            (file, binding.original_name.as_str(), 0)
        };
        let declarations = symbol_map
            .get(name)?
            .iter()
            .filter(|(candidate, symbol)| *candidate == file && type_kind(symbol.kind))
            .count();
        return (declarations == 1).then(|| TypedReceiverOrigin {
            file: file.into(),
            name: name.into(),
            rust_class_position: None,
        });
    }
    if !imports.is_empty() {
        return None;
    }
    if matches!(language, Language::Kotlin | Language::Java) {
        return jvm_type_origin(source_file, type_name, symbol_map, graph, type_kind);
    }
    // Resolved globs may expose a class even without a named alias record.
    let imports = graph.imports_of(source_file);
    let reachable: Vec<_> = symbol_map
        .get(type_name)
        .into_iter()
        .flatten()
        .filter(|(file, symbol)| {
            type_kind(symbol.kind)
                && imports.iter().any(|(_, target)| target == file)
                && (symbol.visibility != Visibility::Private
                    || language == Language::Rust && graph.resolves_parent(source_file, file))
        })
        .collect();
    (reachable.len() == 1).then(|| TypedReceiverOrigin {
        file: reachable[0].0.into(),
        name: type_name.into(),
        rust_class_position: None,
    })
}

/// The Kotlin/Java declaration a type name in `source_file` denotes, by the
/// languages' precedence: a single-type import, then a top-level type of the
/// same package, then an on-demand import (JLS 6.4.1; Kotlin: explicit
/// import > same package > star import). `Foo f = new Foo(); f.hit();` in
/// package `com.b` with `import com.a.*;` is `com.b.Foo`, and with no import
/// at all a same-package `Foo` is still a known origin. Ambiguity at the
/// winning tier is no origin.
fn jvm_type_origin(
    source_file: &str,
    type_name: &str,
    symbol_map: &std::collections::HashMap<String, Vec<(&str, &RawSymbol)>>,
    graph: &ImportGraph,
    type_kind: impl Fn(SymbolKind) -> bool,
) -> Option<TypedReceiverOrigin> {
    let imports = graph.imports_of(source_file);
    let jvm_imports = graph.jvm_imports_of(source_file);
    let mut best: Option<(u8, Vec<&str>)> = None;
    for (file, symbol) in symbol_map.get(type_name).into_iter().flatten() {
        if *file == source_file || !type_kind(symbol.kind) {
            continue;
        }
        let import_rank = (symbol.visibility != Visibility::Private
            && imports.iter().any(|(_, target)| target == file))
        .then(|| {
            jvm_imports.map_or(Some(JvmImportRank::OnDemand), |jvm| {
                jvm.rank(file, symbol.start_line, Some(type_name))
            })
        })
        .flatten();
        let top_level = symbol.parent_name.is_none();
        let rank = match import_rank {
            Some(JvmImportRank::Declaration) => 0,
            _ if top_level && graph.jvm_same_package(source_file, file) => 1,
            Some(JvmImportRank::OnDemand) if top_level => 2,
            _ => continue,
        };
        if best.as_ref().is_none_or(|(best_rank, _)| rank < *best_rank) {
            best = Some((rank, vec![*file]));
        } else if let Some((best_rank, files)) = best.as_mut()
            && *best_rank == rank
        {
            files.push(*file);
        }
    }
    let (_, files) = best?;
    (files.len() == 1).then(|| TypedReceiverOrigin {
        file: files[0].into(),
        name: type_name.into(),
        rust_class_position: None,
    })
}

/// Bound on nested receiver expressions followed by [`ReceiverInference::infer`]
/// (`a.b().c().d()` is three levels).
const RECEIVER_INFERENCE_DEPTH: u8 = 4;

/// Exact Go owner evidence travels with each chain step. Other languages
/// retain their existing name-based inference until they have equivalent
/// declaration evidence; no global name match can manufacture a Go origin.
#[derive(Clone, PartialEq, Eq)]
struct InferredReceiverType {
    name: String,
    origin: Option<TypedReceiverOrigin>,
}

impl InferredReceiverType {
    fn unscoped(name: String) -> Self {
        Self { name, origin: None }
    }

    fn binding(self, line: u32) -> crate::type_extractors::TypeBinding {
        let binding = crate::type_extractors::TypeBinding {
            type_name: self.name,
            line,
            confidence: 0.85,
            source: crate::type_extractors::BindingSource::ReturnType,
        };
        match self.origin {
            Some(origin) => binding.scoped_return(&origin.file, line),
            None => binding,
        }
    }
}

/// Everything [`ReceiverInference::infer`] reads. Bundled so the recursion does
/// not thread ten parameters.
struct ReceiverInference<'a, 'b, 'c> {
    file_path: &'a str,
    reference: &'a RawReference,
    scope_index: &'a FileScopeIndex<'c>,
    symbol_map: &'a std::collections::HashMap<String, Vec<(&'b str, &'b RawSymbol)>>,
    extends_map: &'a std::collections::HashMap<String, Vec<String>>,
    graph: &'a ImportGraph,
    language: Language,
    env: Option<&'a TypeEnvironment>,
}

/// Whether a receiver is itself a call or construction expression
/// (`new K()`, `K.make()`, `k.m1()`, `New(1)`), as opposed to a name or a
/// field path. Only these get receiver-expression inference.
fn is_expression_receiver(receiver: &str) -> bool {
    let receiver = receiver.trim();
    receiver.ends_with(')') || receiver.starts_with("new ")
}

/// `callee` of a trailing call `callee(args)`, or `None` when `expr` does not
/// end in one balanced argument list.
fn split_trailing_call(expr: &str) -> Option<&str> {
    if !expr.ends_with(')') {
        return None;
    }
    let mut depth = 0i32;
    for (index, ch) in expr.char_indices().rev() {
        match ch {
            ')' => depth += 1,
            '(' => {
                depth -= 1;
                if depth == 0 {
                    return Some(expr[..index].trim_end());
                }
            }
            _ => {}
        }
    }
    None
}

/// Drop trailing explicit type arguments from a callee: `f::<T>`, `f<T>`,
/// Go's `Sum[int]`.
fn strip_type_arguments(callee: &str) -> &str {
    let mut callee = callee.trim_end();
    for (open, close) in [('<', '>'), ('[', ']')] {
        if callee.ends_with(close) {
            let mut depth = 0i32;
            for (index, ch) in callee.char_indices().rev() {
                if ch == close {
                    depth += 1;
                } else if ch == open {
                    depth -= 1;
                    if depth == 0 {
                        callee = callee[..index].trim_end();
                        break;
                    }
                }
            }
        }
    }
    callee.strip_suffix("::").unwrap_or(callee)
}

/// Split `base.member` at its last top-level member separator (`.`, `?.`,
/// `::`, `->`, `?->`). `None` when there is no separator outside brackets.
fn split_member(expr: &str) -> Option<(&str, &str)> {
    let bytes = expr.as_bytes();
    let mut depth = 0i32;
    let mut index = bytes.len();
    while index > 0 {
        index -= 1;
        match bytes[index] {
            b')' | b']' | b'}' => depth += 1,
            b'(' | b'[' | b'{' => depth -= 1,
            _ => {}
        }
        if depth != 0 {
            continue;
        }
        let previous = index.checked_sub(1).map(|at| bytes[at]);
        let (base_end, member_start) = match (bytes[index], previous) {
            (b'.', _) => (index, index + 1),
            (b':', Some(b':')) | (b'>', Some(b'-')) => (index - 1, index + 1),
            _ => continue,
        };
        let base = expr[..base_end].trim_end().trim_end_matches('?').trim_end();
        return Some((base, expr[member_start..].trim()));
    }
    None
}

fn is_identifier(text: &str) -> bool {
    !text.is_empty()
        && text
            .chars()
            .all(|ch| ch.is_alphanumeric() || ch == '_' || ch == '$')
        && !text.starts_with(|ch: char| ch.is_ascii_digit())
}

/// The type a receiver binding's internal tag names (`file#Type#line`,
/// `Type@position`), as a bare type name.
fn untagged_type(type_name: &str) -> &str {
    let name = TypeNameParts::decode(type_name).raw_type;
    name.split('@').next().unwrap_or(name)
}

fn is_type_symbol(symbol: &RawSymbol) -> bool {
    matches!(
        symbol.kind,
        SymbolKind::Class
            | SymbolKind::Enum
            | SymbolKind::Interface
            | SymbolKind::Trait
            | SymbolKind::Extension
    )
}

/// Normalize a recorded return type to a repository type name. Wrappers
/// whose methods are not the inner type's (`Option<T>`, `Promise<T>`,
/// `Vec<T>`) yield `None`; smart pointers deref to their inner type.
fn normalize_return_type(
    raw: &str,
    owner: Option<&str>,
    symbol_map: &std::collections::HashMap<String, Vec<(&str, &RawSymbol)>>,
) -> Option<String> {
    let mut text = raw.trim();
    text = text.trim_start_matches(':').trim_start_matches("->").trim();
    // Defensive: a recorded type never includes a body or expression body.
    text = text[..text.find(['{', '=', ';']).unwrap_or(text.len())].trim();
    loop {
        let before = text;
        for prefix in ["&mut ", "&", "*", "mut ", "?", "impl ", "dyn "] {
            text = text.strip_prefix(prefix).unwrap_or(text).trim_start();
        }
        // A Rust lifetime (`&'a Foo`).
        if let Some(rest) = text.strip_prefix('\'') {
            text = rest
                .split_once(char::is_whitespace)
                .map_or("", |(_, rest)| rest)
                .trim_start();
        }
        if text == before {
            break;
        }
    }
    text = text.trim_end_matches(['?', '!']).trim_end();
    if let Some((outer, rest)) = text.split_once('<') {
        if !matches!(outer.trim(), "Box" | "Rc" | "Arc") {
            return None;
        }
        let inner = rest.strip_suffix('>')?;
        return normalize_return_type(inner, owner, symbol_map);
    }
    let name = match text {
        "Self" | "self" | "static" | "this" | "$this" => owner?,
        other => other.rsplit(['.', ':', '\\']).next().unwrap_or(other),
    };
    (is_identifier(name)
        && symbol_map
            .get(name)
            .is_some_and(|symbols| symbols.iter().any(|(_, symbol)| is_type_symbol(symbol))))
    .then(|| name.to_string())
}

impl ReceiverInference<'_, '_, '_> {
    /// The static type of a receiver expression, or `None` when nothing in
    /// the graph establishes one. Every step needs exact evidence: a `new`
    /// of a repository type, a declared return type, or a typed binding.
    fn infer(&self, expr: &str, depth: u8) -> Option<InferredReceiverType> {
        if depth > RECEIVER_INFERENCE_DEPTH {
            return None;
        }
        let expr = expr.trim();
        if let Some(rest) = expr.strip_prefix("new ") {
            let path: String = rest
                .trim_start()
                .chars()
                .take_while(|ch| {
                    ch.is_alphanumeric() || matches!(*ch, '_' | '$' | '.' | ':' | '\\')
                })
                .collect();
            let name = path.rsplit(['.', ':', '\\']).next()?;
            return self
                .type_exists(name)
                .then(|| InferredReceiverType::unscoped(name.to_string()));
        }
        if let Some(callee) = split_trailing_call(expr) {
            if callee.is_empty() {
                // A parenthesized receiver: PHP `(new Foo())->bar()`.
                return self.infer(&expr[1..expr.len() - 1], depth + 1);
            }
            let callee = strip_type_arguments(callee);
            if callee.starts_with("new ") {
                return self.infer(callee, depth + 1);
            }
            return match split_member(callee) {
                Some((base, method)) if is_identifier(method) => {
                    self.member_call_type(base, method, depth)
                }
                Some(_) => None,
                None if is_identifier(callee) => self.free_call_type(callee),
                None => None,
            };
        }
        if depth > 0 && is_identifier(expr) {
            return self.identifier_type(expr);
        }
        None
    }

    fn type_exists(&self, name: &str) -> bool {
        is_identifier(name)
            && self
                .symbol_map
                .get(name)
                .is_some_and(|symbols| symbols.iter().any(|(_, symbol)| is_type_symbol(symbol)))
    }

    /// Whether `file` is visible from the reference's file without a name
    /// the receiver does not carry: itself, a sibling in the same directory
    /// (a Go package, a Python package), or an import target. A Go import
    /// already records every file of its package; only a Python package
    /// import also reaches the package's sibling modules.
    fn file_visible(&self, file: &str) -> bool {
        file == self.file_path
            || parent_dir(file) == parent_dir(self.file_path)
            || self
                .graph
                .imports_of(self.file_path)
                .iter()
                .any(|(_, target)| {
                    target == file
                        || self.language == Language::Python
                            && parent_dir(target) == parent_dir(file)
                })
    }

    /// `file_visible`, minus a Go package imported under a qualifier: a bare
    /// Go name never reaches it (see `candidate_matches`).
    fn bare_name_visible(&self, file: &str) -> bool {
        self.file_visible(file)
            && !(self.language == Language::Go
                && self.graph.go_requires_qualifier(self.file_path, file))
    }

    fn identifier_type(&self, name: &str) -> Option<InferredReceiverType> {
        let env = self.env?;
        let binding = if is_self_receiver(name) {
            env.lookup_self(self.reference.start_line)
        } else if let Some(call_scope) = self.reference.scope {
            let declaration =
                active_lexical_binding(self.scope_index, self.reference, name, self.language)?;
            env.lookup_declaration(name, declaration.scope?, call_scope.position)
        } else {
            env.lookup(name, self.reference.start_line)
        }?;
        let parts = binding.type_name_parts();
        let name = untagged_type(&binding.type_name);
        if self.language == Language::Go {
            return self.declared_type(
                parts.origin_file.unwrap_or(self.file_path),
                parts.raw_type,
                None,
            );
        }
        self.type_exists(name)
            .then(|| InferredReceiverType::unscoped(name.to_string()))
    }

    /// A free call `f(...)`: a constructor-style call of a repository type
    /// (`Child()`, `Point(1, 2)`), or a function with one declared return
    /// type among the visible declarations.
    fn free_call_type(&self, callee: &str) -> Option<InferredReceiverType> {
        if active_lexical_binding(self.scope_index, self.reference, callee, self.language).is_some()
        {
            return None;
        }
        let symbols = self.symbol_map.get(callee)?;
        let functions: Vec<_> = symbols
            .iter()
            .filter(|(file, symbol)| {
                symbol.kind == SymbolKind::Function
                    && symbol.parent_name.is_none()
                    && self.bare_name_visible(file)
            })
            .copied()
            .collect();
        if functions.is_empty() {
            let types: Vec<_> = symbols
                .iter()
                .filter(|(file, symbol)| {
                    matches!(symbol.kind, SymbolKind::Class | SymbolKind::Enum)
                        && self.bare_name_visible(file)
                })
                .collect();
            return if self.language == Language::Go {
                self.declared_type(self.file_path, callee, None)
            } else {
                (!types.is_empty()).then(|| InferredReceiverType::unscoped(callee.to_string()))
            };
        }
        let local: Vec<_> = functions
            .iter()
            .filter(|(file, _)| *file == self.file_path)
            .copied()
            .collect();
        let candidates: Vec<_> = if local.is_empty() { functions } else { local };
        self.agreed_return_type(candidates.into_iter(), None)
    }

    /// `base.method(...)`: a static call when `base` names a repository type
    /// that is not shadowed by a local, otherwise a call on `base`'s own
    /// inferred type. Inherited methods are found through `extends`.
    fn member_call_type(
        &self,
        base: &str,
        method: &str,
        depth: u8,
    ) -> Option<InferredReceiverType> {
        let base = base.trim();
        let owner = if is_identifier(base)
            && !is_self_receiver(base)
            && self.type_exists(base)
            && active_lexical_binding(self.scope_index, self.reference, base, self.language)
                .is_none()
            && self.identifier_type(base).is_none()
        {
            if self.language == Language::Go {
                self.declared_type(self.file_path, base, None)
            } else {
                Some(InferredReceiverType::unscoped(base.to_string()))
            }
        } else if is_identifier(base) && !is_self_receiver(base) && !self.type_exists(base) {
            // A namespace or package qualifier (`calc.New(3)`): a free
            // function of the module or package that qualifier imports.
            match self.identifier_type(base) {
                Some(owner) => Some(owner),
                None => return self.qualified_free_call_type(base, method),
            }
        } else if is_identifier(base) {
            self.identifier_type(base)
        } else {
            self.infer(base, depth + 1)
        }?;
        self.method_return_type(&owner, method)
    }

    /// `qualifier.function(...)` where `qualifier` names an imported module
    /// or package, not a value. Go uses the same package-qualifier table as
    /// `candidate_matches` (`import c "…/calc"` binds `c` to every file of
    /// `calc`); elsewhere a whole-module binding (`import pkg.helpers as h`,
    /// `import * as m from`) names its one source file.
    fn qualified_free_call_type(
        &self,
        qualifier: &str,
        function: &str,
    ) -> Option<InferredReceiverType> {
        if self.language == Language::Go
            && active_lexical_binding(self.scope_index, self.reference, qualifier, self.language)
                .is_some()
        {
            return None;
        }
        let files: Vec<&str> = if self.language == Language::Go {
            self.graph
                .go_qualified_package(self.file_path, qualifier)?
                .iter()
                .map(String::as_str)
                .collect()
        } else {
            self.graph
                .bindings_named(self.file_path, qualifier)
                .filter(|binding| binding.original_name == "*")
                .filter_map(|binding| binding.source_file.as_deref())
                .collect()
        };
        let functions: Vec<_> = self
            .symbol_map
            .get(function)?
            .iter()
            .filter(|(file, symbol)| {
                symbol.kind == SymbolKind::Function
                    && symbol.parent_name.is_none()
                    && files.contains(file)
                    && (self.language != Language::Go
                        || symbol.visibility != Visibility::Private
                        || parent_dir(self.file_path) == parent_dir(file)
                            && go_same_package(self.graph, self.file_path, file))
            })
            .copied()
            .collect();
        if functions.is_empty() {
            return None;
        }
        self.agreed_return_type(functions.into_iter(), None)
    }

    fn method_return_type(
        &self,
        receiver: &InferredReceiverType,
        method: &str,
    ) -> Option<InferredReceiverType> {
        let mut current = vec![receiver.name.clone()];
        let mut visited = std::collections::HashSet::new();
        for _ in 0..=RECEIVER_INFERENCE_DEPTH {
            let mut next = Vec::new();
            for owner in &current {
                if !visited.insert(owner.clone()) {
                    continue;
                }
                let methods: Vec<_> = self
                    .symbol_map
                    .get(method)
                    .into_iter()
                    .flatten()
                    .filter(|(file, symbol)| {
                        symbol.kind == SymbolKind::Method
                            && symbol.parent_name.as_deref() == Some(owner.as_str())
                            && (self.language != Language::Go
                                || receiver.origin.as_ref().is_some_and(|origin| {
                                    go_method_origin_matches(
                                        file,
                                        origin,
                                        self.graph,
                                        self.symbol_map,
                                    ) && (symbol.visibility != Visibility::Private
                                        || parent_dir(self.file_path) == parent_dir(file)
                                            && go_same_package(self.graph, self.file_path, file))
                                }))
                    })
                    .copied()
                    .collect();
                if !methods.is_empty() {
                    return self.agreed_return_type(methods.into_iter(), Some(owner.as_str()));
                }
                if let Some(parents) = self.extends_map.get(owner) {
                    next.extend(parents.iter().cloned());
                }
            }
            if next.is_empty() {
                break;
            }
            current = next;
        }
        None
    }

    /// Interpret a return relative to its declaration before dropping qualifiers.
    fn declared_type(
        &self,
        file: &str,
        raw: &str,
        owner: Option<&str>,
    ) -> Option<InferredReceiverType> {
        if self.language != Language::Go {
            return normalize_return_type(raw, owner, self.symbol_map)
                .map(InferredReceiverType::unscoped);
        }
        // Parser return metadata already unwraps named scalar results. Refuse
        // tuples, containers and expressions rather than borrowing their names.
        let name = raw.trim().trim_start_matches('*').trim();
        let origin = typed_receiver_origin(
            file,
            name,
            self.reference,
            self.symbol_map,
            self.graph,
            self.language,
            None,
            false,
        )?;
        Some(InferredReceiverType {
            name: origin.name.clone(),
            origin: Some(origin),
        })
    }

    /// The one normalized return type every candidate declares, or `None`
    /// when any lacks one or they disagree.
    fn agreed_return_type<'s>(
        &self,
        candidates: impl Iterator<Item = (&'s str, &'s RawSymbol)>,
        owner: Option<&str>,
    ) -> Option<InferredReceiverType> {
        let mut agreed: Option<InferredReceiverType> = None;
        for (file, symbol) in candidates {
            let raw = symbol.type_info.as_ref()?.return_type.as_deref()?;
            let owner = owner.or(symbol.parent_name.as_deref());
            let normalized = self.declared_type(file, raw, owner)?;
            match &agreed {
                Some(existing) if *existing != normalized => return None,
                _ => agreed = Some(normalized),
            }
        }
        agreed
    }
}

/// The leading name of a receiver chain: `Child` in `Child.make()`, `K` in
/// `new K()`, `New` in `New(1)`.
fn receiver_chain_head(receiver: &str) -> &str {
    let receiver = receiver.trim();
    let receiver = receiver
        .strip_prefix("new ")
        .unwrap_or(receiver)
        .trim_start();
    let end = receiver
        .find(|ch: char| !(ch.is_alphanumeric() || ch == '_' || ch == '$'))
        .unwrap_or(receiver.len());
    &receiver[..end]
}

/// Last-resort target for a method called on an expression whose type no
/// evidence establishes (`Child.make().run()` with an unannotated `make`).
/// Linked only when
///
/// - the chain starts at a repository TYPE visible from this file and not
///   shadowed by a local (so `new Map().get(k)` or `fetch().then()` never
///   reach this), and
/// - exactly one method of that name exists in the whole repository, no free
///   function shares the name, it is accessible from this file, and
/// - every step of the chain is typed by evidence the fallback accepts (see
///   [`fallback_receiver_type`]), and the receiver's resulting type is the
///   method's owner or a subtype of it.
///
/// A function head (`q().get(k)`) is refused: an unannotated repo function
/// can return anything, including an external type, so the repository's one
/// `get` is no evidence (the nw-150 `buildQuery().where(..)` shape). A common
/// name such as `get` that is declared twice anywhere links nothing. A static
/// method declared to return a library type (`Optional<A>`, `List<A>`,
/// `Promise<Q[]>`, `Q[]`) or left unannotated without returning a
/// construction of a repository type links nothing either: `A.find().get()`
/// is `Optional.get`, not `A.get`.
fn unique_expression_method<'s>(
    inference: &ReceiverInference<'_, 's, '_>,
    returned_constructions: &ReturnedConstructions,
    receiver: &str,
    method: &str,
    source: &RawSymbol,
) -> Option<(&'s str, &'s RawSymbol)> {
    let head = receiver_chain_head(receiver);
    if !is_identifier(head)
        || is_self_receiver(head)
        || active_lexical_binding(
            inference.scope_index,
            inference.reference,
            head,
            inference.language,
        )
        .is_some()
        || !inference.symbol_map.get(head).is_some_and(|symbols| {
            symbols
                .iter()
                .any(|(file, symbol)| is_type_symbol(symbol) && inference.bare_name_visible(file))
        })
    {
        return None;
    }
    let symbols = inference.symbol_map.get(method)?;
    // Any free function of that name makes the spelling ambiguous enough to
    // refuse: the uniqueness is about the NAME, not just about methods.
    if symbols
        .iter()
        .any(|(_, symbol)| symbol.kind == SymbolKind::Function)
    {
        return None;
    }
    let mut methods = symbols
        .iter()
        .filter(|(_, symbol)| symbol.kind == SymbolKind::Method);
    let (file, symbol) = *methods.next()?;
    if methods.next().is_some()
        || !inference.file_visible(file)
        || !typed_member_accessible(
            inference.file_path,
            file,
            source,
            symbol,
            inference.language,
            inference.graph,
        )
    {
        return None;
    }
    // The receiver's type must be the method's owner or a subtype of it.
    let owner = symbol.parent_name.as_deref()?;
    let receiver_type = fallback_receiver_type(inference, returned_constructions, receiver, 0)?;
    is_same_or_subtype(inference.extends_map, &receiver_type, owner).then_some((file, symbol))
}

/// Whether `ty` is `ancestor` or reaches it through `extends`.
fn is_same_or_subtype(
    extends_map: &std::collections::HashMap<String, Vec<String>>,
    ty: &str,
    ancestor: &str,
) -> bool {
    let mut current = vec![ty.to_string()];
    let mut visited = std::collections::HashSet::new();
    for _ in 0..=RECEIVER_INFERENCE_DEPTH {
        if current.iter().any(|ty| ty == ancestor) {
            return true;
        }
        let mut next = Vec::new();
        for ty in current {
            if visited.insert(ty.clone())
                && let Some(parents) = extends_map.get(&ty)
            {
                next.extend(parents.iter().cloned());
            }
        }
        if next.is_empty() {
            return false;
        }
        current = next;
    }
    false
}

/// The repository type the unique-method fallback may assume a receiver
/// expression evaluates to, or `None` to refuse the link.
///
/// Exact inference ([`ReceiverInference::infer`]) is tried first. Beyond it
/// only a static call on a repository type (`Child.make()`, or a call on an
/// expression this function itself typed) is followed, and its method must
///
/// - declare a return type that names a repository type (a declared
///   `Optional<A>`, `List<A>`, `Promise<Q[]>`, `Q[]`, `dict` or `void` is
///   evidence AGAINST the link, never for it), or
/// - declare none, not be `async`, and return a construction of exactly one
///   repository type: `return Child()`, `return new K()`, or Python's
///   `return cls(..)` (the static receiver type).
///
/// An unannotated method with no such return (`return json.loads(s)`) can
/// return anything, so it is refused like an unannotated free function.
fn fallback_receiver_type(
    inference: &ReceiverInference<'_, '_, '_>,
    returned_constructions: &ReturnedConstructions,
    expr: &str,
    depth: u8,
) -> Option<String> {
    if depth > RECEIVER_INFERENCE_DEPTH {
        return None;
    }
    if let Some(inferred) = inference.infer(expr, depth) {
        return Some(inferred.name);
    }
    let expr = expr.trim();
    let callee = split_trailing_call(expr)?;
    if callee.is_empty() {
        return fallback_receiver_type(
            inference,
            returned_constructions,
            &expr[1..expr.len() - 1],
            depth + 1,
        );
    }
    let callee = strip_type_arguments(callee);
    let (base, method) = split_member(callee)?;
    if !is_identifier(method) {
        return None;
    }
    let base = base.trim();
    let owner = if is_identifier(base) {
        // Only a static call on a repository type; a value or a module
        // qualifier is `infer`'s business, and it found nothing.
        (!is_self_receiver(base)
            && inference.type_exists(base)
            && active_lexical_binding(
                inference.scope_index,
                inference.reference,
                base,
                inference.language,
            )
            .is_none()
            && inference.identifier_type(base).is_none())
        .then(|| base.to_string())?
    } else {
        fallback_receiver_type(inference, returned_constructions, base, depth + 1)?
    };
    fallback_method_return_type(inference, returned_constructions, &owner, method)
}

/// The return type of `receiver_type.method(..)` under the evidence rules of
/// [`fallback_receiver_type`]. The method is looked up on `receiver_type`
/// first and then on its ancestors, nearest level wins.
fn fallback_method_return_type(
    inference: &ReceiverInference<'_, '_, '_>,
    returned_constructions: &ReturnedConstructions,
    receiver_type: &str,
    method: &str,
) -> Option<String> {
    let mut current = vec![receiver_type.to_string()];
    let mut visited = std::collections::HashSet::new();
    for _ in 0..=RECEIVER_INFERENCE_DEPTH {
        let mut next = Vec::new();
        for owner in &current {
            if !visited.insert(owner.clone()) {
                continue;
            }
            let methods: Vec<(&str, &RawSymbol)> = inference
                .symbol_map
                .get(method)
                .into_iter()
                .flatten()
                .filter(|(_, symbol)| {
                    symbol.kind == SymbolKind::Method
                        && symbol.parent_name.as_deref() == Some(owner.as_str())
                })
                .copied()
                .collect();
            if !methods.is_empty() {
                return agreed_fallback_return(
                    inference,
                    returned_constructions,
                    &methods,
                    owner,
                    receiver_type,
                );
            }
            if let Some(parents) = inference.extends_map.get(owner) {
                next.extend(parents.iter().cloned());
            }
        }
        if next.is_empty() {
            break;
        }
        current = next;
    }
    None
}

/// The one type every declaration of a method returns, under the rules of
/// [`fallback_receiver_type`]. `owner` is the declaring type (`Self`);
/// `receiver_type` is what a Python `cls(..)` constructs.
fn agreed_fallback_return(
    inference: &ReceiverInference<'_, '_, '_>,
    returned_constructions: &ReturnedConstructions,
    methods: &[(&str, &RawSymbol)],
    owner: &str,
    receiver_type: &str,
) -> Option<String> {
    let mut agreed: Option<String> = None;
    for (file, symbol) in methods {
        let declared = symbol
            .type_info
            .as_ref()
            .and_then(|info| info.return_type.as_deref())
            .map(|raw| raw.trim().trim_matches(['"', '\'']).trim())
            .filter(|raw| !raw.is_empty());
        let returned = match declared {
            // A declared type that is not a repository type (a wrapper, a
            // collection, a primitive, `void`) refuses: `normalize_return_type`
            // yields `None` for it.
            Some(raw) => normalize_return_type(raw, Some(owner), inference.symbol_map)?,
            None => {
                if symbol
                    .signature
                    .split(|ch: char| !(ch.is_alphanumeric() || ch == '_'))
                    .any(|token| token == "async")
                {
                    return None;
                }
                let names = returned_constructions.get(&(file.to_string(), symbol.start_line))?;
                let mut constructed: Option<String> = None;
                for name in names {
                    let ty = if name == "cls" && inference.language == Language::Python {
                        receiver_type.to_string()
                    } else if inference.type_exists(name) {
                        name.clone()
                    } else {
                        return None;
                    };
                    match &constructed {
                        Some(existing) if *existing != ty => return None,
                        _ => constructed = Some(ty),
                    }
                }
                constructed?
            }
        };
        match &agreed {
            Some(existing) if *existing != returned => return None,
            _ => agreed = Some(returned),
        }
    }
    agreed
}

/// Go methods and their receiver declarations may occupy sibling files. When
/// an incremental pass omits a package clause, a unique named type declaration
/// must still anchor ownership; a same-spelled standalone type is not evidence.
fn go_method_origin_matches(
    file: &str,
    origin: &TypedReceiverOrigin,
    graph: &ImportGraph,
    symbol_map: &std::collections::HashMap<String, Vec<(&str, &RawSymbol)>>,
) -> bool {
    if nestweaver_parser::detect_language(std::path::Path::new(file)) != Some(Language::Go)
        || nestweaver_parser::detect_language(std::path::Path::new(&origin.file))
            != Some(Language::Go)
        || parent_dir(file) != parent_dir(&origin.file)
        || !go_same_package(graph, file, &origin.file)
    {
        return false;
    }
    if graph.go_package_of(file).is_some() && graph.go_package_of(&origin.file).is_some() {
        return true;
    }
    // Both internal and external test packages use *_test.go. Unknown package
    // metadata cannot prove a cross-file route involving either test side.
    if file.ends_with("_test.go") || origin.file.ends_with("_test.go") {
        return false;
    }
    let mut owners =
        symbol_map
            .get(&origin.name)
            .into_iter()
            .flatten()
            .filter(|(owner_file, owner)| {
                parent_dir(owner_file) == parent_dir(&origin.file)
                    && nestweaver_parser::detect_language(std::path::Path::new(owner_file))
                        == Some(Language::Go)
                    && matches!(
                        owner.kind,
                        SymbolKind::Class
                            | SymbolKind::Enum
                            | SymbolKind::TypeAlias
                            | SymbolKind::Interface
                    )
            });
    owners
        .next()
        .is_some_and(|(owner_file, _)| *owner_file == origin.file)
        && owners.next().is_none()
}

fn method_belongs_to_origin(
    file: &str,
    symbol: &RawSymbol,
    origin: &TypedReceiverOrigin,
    graph: &ImportGraph,
    symbol_map: &std::collections::HashMap<String, Vec<(&str, &RawSymbol)>>,
    type_envs: Option<&std::collections::HashMap<String, TypeEnvironment>>,
) -> bool {
    symbol.parent_name.as_deref() == Some(origin.name.as_str())
        && (go_method_origin_matches(file, origin, graph, symbol_map)
            || file == origin.file
                && origin.rust_class_position.is_none_or(|position| {
                    let owners = type_envs.and_then(|envs| envs.get(file)).and_then(|env| {
                        env.rust_method_owners
                            .get(&(symbol.name.clone(), symbol.start_line))
                    });
                    owners.is_some_and(|owners| owners.len() == 1 && owners[0] == position)
                })
            || !symbol_map
                .get(&origin.name)
                .into_iter()
                .flatten()
                .any(|(owner_file, owner)| {
                    *owner_file == file
                        && matches!(owner.kind, SymbolKind::Class | SymbolKind::Enum)
                })
                && graph.bindings_named(file, &origin.name).any(|binding| {
                    binding.original_name == origin.name
                        && binding.source_file.as_deref() == Some(origin.file.as_str())
                }))
}

fn typed_member_accessible(
    source_file: &str,
    target_file: &str,
    source: &RawSymbol,
    target: &RawSymbol,
    language: Language,
    graph: &ImportGraph,
) -> bool {
    if target.kind != SymbolKind::Method {
        return false;
    }
    let prefix = target.signature.split('(').next().unwrap_or("");
    if prefix
        .split_whitespace()
        .rev()
        .skip(1)
        .any(|token| matches!(token, "private" | "protected"))
        || target.name.starts_with('#')
    {
        return source_file == target_file
            && source.parent_name.is_some()
            && source.parent_name == target.parent_name;
    }
    target.visibility != Visibility::Private
        || source_file == target_file
        || (language == Language::Go
            && parent_dir(source_file) == parent_dir(target_file)
            && go_same_package(graph, source_file, target_file))
        || (language == Language::Rust && graph.resolves_parent(source_file, target_file))
}

fn candidate_matches(
    reference: &RawReference,
    source_file: &str,
    candidate_file: &str,
    sym: &RawSymbol,
    language: Language,
    graph: &ImportGraph,
) -> bool {
    if source_file != candidate_file
        && sym.visibility == Visibility::Private
        && !(language == Language::Go
            && parent_dir(source_file) == parent_dir(candidate_file)
            && go_same_package(graph, source_file, candidate_file))
    {
        return false;
    }
    match reference.kind {
        ReferenceKind::Macro => {
            if !is_macro_symbol(sym) {
                return false;
            }
        }
        ReferenceKind::Call => {
            if !is_callable_symbol(sym) {
                return false;
            }
        }
        ReferenceKind::ReadAccess | ReferenceKind::WriteAccess
            if source_file == candidate_file
                && matches!(
                    sym.kind,
                    SymbolKind::Property
                        | SymbolKind::Constant
                        | SymbolKind::Variable
                        | SymbolKind::TypeAlias
                ) =>
        {
            return true;
        }
        _ => {}
    }
    if language == Language::Go {
        // A Go import binds a package qualifier: `calc.New` names a
        // top-level `New` in any file of the package `calc` names, and a
        // bare `New` never names an imported (non-dot) package's file.
        match reference.receiver.as_deref() {
            Some(receiver) => {
                if let Some(files) = graph.go_qualified_package(source_file, receiver) {
                    return sym.parent_name.is_none()
                        && files.iter().any(|file| file == candidate_file);
                }
            }
            None => {
                if graph.go_requires_qualifier(source_file, candidate_file) {
                    return false;
                }
            }
        }
    }
    match reference.receiver.as_deref() {
        None => true,
        Some(receiver) if is_path_receiver(receiver) => path_denotes(candidate_file, sym, receiver),
        Some(receiver) if is_self_receiver(receiver) => source_file == candidate_file,
        Some(receiver) => receiver_denotes(candidate_file, sym, Some(receiver)),
    }
}

/// nw-789: a bare JS/TS call never reaches a method, getter or setter
/// declared in this file or a sibling one; those need a receiver
/// (`this.x()`, `obj.x()`). Applied where the reference is the real call:
/// the import tiers keep binding a method an import names (a destructured
/// CommonJS export, or an imported member resolved with its receiver dropped).
fn bare_call_names_member(reference: &RawReference, sym: &RawSymbol, language: Language) -> bool {
    reference.kind == ReferenceKind::Call
        && reference.receiver.is_none()
        && sym.kind == SymbolKind::Method
        && matches!(language, Language::JavaScript | Language::TypeScript)
}

fn is_callable_symbol(sym: &RawSymbol) -> bool {
    matches!(
        sym.kind,
        SymbolKind::Function | SymbolKind::Method | SymbolKind::Class | SymbolKind::Variable
    )
}

fn is_macro_symbol(sym: &RawSymbol) -> bool {
    let signature = sym.signature.as_str();
    signature.contains("macro_rules!") || signature.contains("proc_macro")
}

fn is_self_receiver(receiver: &str) -> bool {
    matches!(receiver, "self" | "this" | "$this")
}

/// A path receiver denotes a candidate when any of its segments is the
/// candidate's file stem (`store::GraphStore` → `store.rs`) or its last
/// segment is the candidate's declaring type.
fn path_denotes(candidate_file: &str, sym: &RawSymbol, qualifier: &str) -> bool {
    let segments: Vec<&str> = qualifier
        .split("::")
        .filter(|segment| !segment.is_empty())
        .collect();
    let stem = candidate_file
        .rsplit('/')
        .next()
        .and_then(|base| base.split('.').next());
    if stem.is_some_and(|stem| segments.contains(&stem)) {
        return true;
    }
    segments
        .last()
        .is_some_and(|last| sym.parent_name.as_deref() == Some(*last))
}

/// A local binding shadows `reference` when it sits in the same enclosing
/// symbol at or before the use, and it is not the declaration of a graph
/// symbol of that name (a `const len = ...` that IS the symbol).
fn active_lexical_binding<'a>(
    scope_index: &FileScopeIndex<'a>,
    reference: &RawReference,
    name: &str,
    language: Language,
) -> Option<&'a RawReference> {
    let at = reference.scope?.position;
    // nw-113 (perf): only the file's scoped bindings of this name, from the
    // per-file index, instead of every binding in the file per reference.
    scope_index
        .named(name)
        .iter()
        .copied()
        .filter(|binding| {
            binding.name == name
                && binding.scope.is_some_and(|scope| {
                    scope.start <= at
                        && at < scope.end
                        && (!matches!(language, Language::Rust | Language::Go)
                            || scope.initialized_at <= at)
                })
        })
        .min_by_key(|binding| {
            let scope = binding.scope.expect("filtered scoped binding");
            (
                scope.end - scope.start,
                scope.hoisted_var && scope.position > at,
                std::cmp::Reverse(if scope.hoisted_var && scope.position > at {
                    0
                } else {
                    scope.position
                }),
            )
        })
}

/// nw-113 (perf): per-file lookups for Priority 1 scoping, built once per
/// file next to `local_bindings`. Before this, every same-file candidate of
/// every bare call scanned all of the file's bindings and then all of its
/// symbols, which is O(calls x candidates x (bindings + symbols)) and
/// measured cubic on a file of many same-named locals (bug-hunt #1).
pub(crate) struct FileScopeIndex<'a> {
    /// Scoped bindings (`LocalBinding`s with a byte scope: declarators,
    /// parameters, and `catch` and `for … of/in` bindings), by name, sorted by
    /// line. A candidate's own declarators are a binary search over its span;
    /// `active_lexical_binding` reads one name's list instead of every binding
    /// in the file.
    declarators: std::collections::HashMap<&'a str, Vec<&'a RawReference>>,
    /// `covering[l - 1]`: how many code-bearing symbols (never the `<module>`
    /// owner) contain line `l`.
    covering: Vec<u32>,
    /// nw-787: each nested JS/TS `function` declaration's visible region, by
    /// name, keyed by the line of its name.
    hoisted: std::collections::HashMap<&'a str, Vec<&'a RawReference>>,
    /// Memo of [`innermost_container`] by the symbol's index in the file's
    /// `sorted_syms` (nw-787 perf): computed once per symbol per file rather
    /// than once per (reference, candidate) pair.
    containers: std::cell::RefCell<std::collections::HashMap<usize, Option<usize>>>,
}

impl<'a> FileScopeIndex<'a> {
    pub(crate) fn build(local_bindings: &[&'a RawReference], sorted_syms: &[&RawSymbol]) -> Self {
        let mut declarators: std::collections::HashMap<&'a str, Vec<&'a RawReference>> =
            std::collections::HashMap::new();
        for binding in local_bindings
            .iter()
            .filter(|binding| binding.scope.is_some())
        {
            declarators
                .entry(binding.name.as_str())
                .or_default()
                .push(binding);
        }
        for bindings in declarators.values_mut() {
            bindings.sort_by_key(|binding| binding.start_line);
        }
        let code: Vec<&&RawSymbol> = sorted_syms.iter().filter(|s| can_contain_code(s)).collect();
        let last = code
            .iter()
            .map(|s| s.end_line.max(s.start_line) as usize)
            .max()
            .unwrap_or(0);
        let mut delta = vec![0i64; last + 1];
        for s in code {
            delta[(s.start_line.max(1) - 1) as usize] += 1;
            delta[s.end_line.max(s.start_line) as usize] -= 1;
        }
        let mut depth = 0i64;
        let covering = delta[..last]
            .iter()
            .map(|change| {
                depth += change;
                depth as u32
            })
            .collect();
        Self {
            declarators,
            covering,
            hoisted: std::collections::HashMap::new(),
            containers: Default::default(),
        }
    }

    /// Adds the file's `HoistedDeclaration`s (nw-787).
    pub(crate) fn with_hoisted(mut self, references: &'a [RawReference]) -> Self {
        for reference in references
            .iter()
            .filter(|reference| reference.kind == ReferenceKind::HoistedDeclaration)
        {
            self.hoisted
                .entry(reference.name.as_str())
                .or_default()
                .push(reference);
        }
        self
    }

    /// The visible region of the nested `function name` declared on `line`.
    /// A graph symbol is keyed by name and line, so declarations that share
    /// both are one symbol: the parser emits no region when one of them is
    /// module-level (the symbol is file-wide), and among nested ones, each
    /// carrying its own declaration offset in `position`, the widest region
    /// wins.
    fn hoisted_region(&self, name: &str, line: u32) -> Option<nestweaver_parser::LexicalScope> {
        self.hoisted
            .get(name)?
            .iter()
            .filter(|reference| reference.start_line == line)
            .filter_map(|reference| reference.scope)
            .max_by_key(|scope| scope.end - scope.start)
    }

    /// [`innermost_container`] of `sym`, memoised. `sorted_syms` must be the
    /// list this index was built from; a symbol not found in it is computed
    /// directly.
    fn innermost_container<'s>(
        &self,
        sorted_syms: &[&'s RawSymbol],
        sym: &RawSymbol,
    ) -> Option<&'s RawSymbol> {
        let lo = sorted_syms.partition_point(|other| other.start_line < sym.start_line);
        let Some(index) = sorted_syms[lo..]
            .iter()
            .take_while(|other| other.start_line == sym.start_line)
            .position(|other| std::ptr::eq(*other, sym))
            .map(|offset| lo + offset)
        else {
            return innermost_container(sorted_syms, sym);
        };
        let container = *self
            .containers
            .borrow_mut()
            .entry(index)
            .or_insert_with(|| {
                innermost_container(sorted_syms, sym).and_then(|container| {
                    sorted_syms
                        .iter()
                        .position(|other| std::ptr::eq(*other, container))
                })
            });
        container.map(|index| sorted_syms[index])
    }

    /// Every scoped binding named `name`, in line order.
    fn named(&self, name: &str) -> &[&'a RawReference] {
        self.declarators.get(name).map_or(&[], Vec::as_slice)
    }

    /// Every scoped binding named `name` (not only declarators: parameters,
    /// `catch` and loop bindings too) whose line lies in `start..=end`. The
    /// caller picks the declaration among them by width.
    fn declarators_in(&self, name: &str, start: u32, end: u32) -> &[&'a RawReference] {
        let Some(bindings) = self.declarators.get(name) else {
            return &[];
        };
        let lo = bindings.partition_point(|binding| binding.start_line < start);
        let hi = bindings.partition_point(|binding| binding.start_line <= end);
        &bindings[lo..hi.max(lo)]
    }

    /// How many code-bearing symbols contain `line`.
    fn covering(&self, line: u32) -> u32 {
        (line as usize)
            .checked_sub(1)
            .and_then(|index| self.covering.get(index))
            .copied()
            .unwrap_or(0)
    }
}

/// nw-113: where a same-file callable `sym` is visible from `reference`.
///
/// A `const f = () => …` declared in a block is a graph `Function` whose
/// declaring `LocalBinding` carries that block's byte range. Returns:
/// - `None` when the declaration is not visible at the call (its own
///   declarator's block excludes the call position);
/// - `Some(width)` with that block's width when it is, so the innermost
///   visible declarator wins;
/// - `Some(usize::MAX)` (unfiltered, base behaviour) for everything that
///   carries no shadowing information: a symbol that is neither a declarator
///   nor a nested function declaration (module-level function declarations;
///   methods; class fields; assignment forms such as `exports.f = function …` and
///   `obj.f = function …`; every non-`Function` kind), and a module-level
///   declarator, which is visible everywhere. All-`MAX` ties keep the base
///   first-match order.
///
/// Module level is decided by symbol nesting: a declarator is module-level
/// when no code-bearing symbol other than `sym` contains its own binding's
/// line. A declarator inside a function, method or test-runner callback is
/// scoped. Residuals:
/// - (base behaviour) a declarator with no enclosing symbol reads as
///   module-level and ties with a module `function f`, falling back to
///   source order: a bare top-level block `{ const f = … }`, an IIFE
///   `(() => { const f = … })()`, or an anonymous top-level callback
///   `app.get('/', () => { const f = … })`;
/// - containment is decided per LINE, so a module-level declarator that
///   shares a line with another symbol's span (a sibling function on the
///   same line, a function's closing-brace line, minified code) counts as
///   scoped, and can then outrank a nested `function` declaration.
///
/// Only a symbol whose signature (after `export`/`declare`/`await`) starts
/// with a declarator keyword (`const`/`let`/`var`/`using`) is scoped. A same-named
/// local declared inside any other symbol's body is not that symbol's
/// declaration and must never scope it. The signature is the node's first
/// line, which for a declaration always starts with that keyword.
///
/// A declarator's own block encloses any same-named local declared inside
/// an initializer in its span (`let a = { g() { let f = 1; return f; } },
/// f = () => 2;` puts the inner `f` ahead of `f`'s own declarator), so among
/// the in-span same-named scoped bindings the one with the WIDEST scope is
/// the symbol's declaration. A same-named parameter, `catch` or loop binding
/// in that span is always narrower than the declarator's own binding, so it
/// is never chosen.
///
/// Gated to bare JS/TS calls (plan validation): Rust has its own exact gate,
/// and receiver calls (`Api.get()`, `this.x()`) are matched by
/// `candidate_matches`, so neither may be filtered here.
///
/// nw-787: a nested `function` declaration is scoped the same way, by the
/// region its `HoistedDeclaration` names (its enclosing function body, or in
/// strict code its block, from the region's start because it is hoisted). A
/// module-level one has none and stays `usize::MAX`. A Python `def` nested in
/// a function or method is visible only inside that function; it carries no
/// byte scope, so its width is its container's line span, which competes only
/// with other Python candidates.
fn same_file_declaration_scope(
    scope_index: &FileScopeIndex<'_>,
    sorted_syms: &[&RawSymbol],
    sym: &RawSymbol,
    reference: &RawReference,
    language: Language,
) -> Option<usize> {
    if reference.receiver.is_some() || sym.kind != SymbolKind::Function {
        return Some(usize::MAX);
    }
    if language == Language::Python {
        if scope_index.covering(sym.start_line) <= 1 {
            return Some(usize::MAX);
        }
        return match scope_index.innermost_container(sorted_syms, sym) {
            Some(container)
                if matches!(container.kind, SymbolKind::Function | SymbolKind::Method) =>
            {
                let end = container.end_line.max(container.start_line);
                (container.start_line <= reference.start_line && reference.start_line <= end)
                    .then(|| (end - container.start_line) as usize)
            }
            _ => Some(usize::MAX),
        };
    }
    if !matches!(language, Language::JavaScript | Language::TypeScript) {
        return Some(usize::MAX);
    }
    let Some(call) = reference.scope else {
        return Some(usize::MAX);
    };
    if !is_declarator_form(&sym.signature) {
        return match scope_index.hoisted_region(&sym.name, sym.start_line) {
            Some(region) => (region.start <= call.position && call.position < region.end)
                .then_some(region.end - region.start),
            None => Some(usize::MAX),
        };
    }
    let Some(declaration) = scope_index
        // The declarator may sit on a later line than the declaration
        // (`const a = 1,\n  b = () => 2;`), so join by the symbol's span,
        // not its first line.
        .declarators_in(&sym.name, sym.start_line, sym.end_line.max(sym.start_line))
        .iter()
        .filter_map(|binding| binding.scope.map(|scope| (binding.start_line, scope)))
        .max_by_key(|(_, scope)| scope.end - scope.start)
    else {
        return Some(usize::MAX);
    };
    let (line, declaration) = declaration;
    if !(declaration.start <= call.position && call.position < declaration.end) {
        return None;
    }
    // Module level: no other code-bearing symbol contains the declarator, so
    // it is visible everywhere and ranks like an unscoped symbol. `sym` is a
    // code-bearing `Function` whose span contains `line` (the join above), so
    // it alone covers the line exactly when no other symbol does.
    let module_level = scope_index.covering(line) <= 1;
    Some(if module_level {
        usize::MAX
    } else {
        declaration.end - declaration.start
    })
}

/// nw-113: whether same-file function `sym` is lexically visible at the call
/// in place of the unresolved import `binding`. Only a `Function` qualifies
/// (a bare call never reaches a method). Its INNERMOST enclosing code-bearing
/// symbol (other than itself) must be a function or method (a lexical
/// function scope, never a class, interface, trait or extension), must
/// contain the call, and must lie strictly within the binding's owner (or
/// anywhere, for a module-level import). Containment is by line, as in
/// `same_file_declaration_scope`'s module-level test.
///
/// nw-787: a JS/TS nested `function` declaration is filtered first by its
/// byte region (`same_file_declaration_scope`) in every pass, so the
/// shapes this line-based test cannot tell apart (a block function called
/// after its block, a function nested in an anonymous callback) never reach
/// it. It still decides a Python nested `def` and any candidate without a
/// region. Residual: when `function f() {} } function h() {` sits on one
/// line, the tie-break credits `f` to `h`.
fn nested_inside_binding(
    scope_index: &FileScopeIndex<'_>,
    sorted_syms: &[&RawSymbol],
    sym: &RawSymbol,
    reference: &RawReference,
    binding: &NamedBinding,
    language: Language,
) -> bool {
    if sym.kind != SymbolKind::Function {
        return false;
    }
    let contains = |outer: &RawSymbol, line: u32| {
        outer.start_line <= line && line <= outer.end_line.max(outer.start_line)
    };
    let same = |a: &RawSymbol, b: &RawSymbol| a.name == b.name && a.start_line == b.start_line;
    let Some(container) = scope_index.innermost_container(sorted_syms, sym) else {
        return false;
    };
    let binding_owner = scoping_owner(sorted_syms, binding.start_line, language);
    matches!(container.kind, SymbolKind::Function | SymbolKind::Method)
        && contains(container, reference.start_line)
        && binding_owner.is_none_or(|owner| {
            !same(owner, container)
                && contains(owner, container.start_line)
                && contains(owner, container.end_line)
        })
}

/// The innermost code-bearing symbol (never `sym` itself or the `<module>`
/// owner) whose line span contains `sym`'s first line: the narrowest span,
/// the later start on a tie, which assumes `sym` follows a shared line's
/// boundary.
fn innermost_container<'a>(
    sorted_syms: &[&'a RawSymbol],
    sym: &RawSymbol,
) -> Option<&'a RawSymbol> {
    sorted_syms
        .iter()
        .take_while(|other| other.start_line <= sym.start_line)
        .filter(|other| {
            !(other.name == sym.name && other.start_line == sym.start_line)
                && can_contain_code(other)
                && other.start_line <= sym.start_line
                && sym.start_line <= other.end_line.max(other.start_line)
        })
        .min_by_key(|other| {
            (
                other.end_line.max(other.start_line) - other.start_line,
                std::cmp::Reverse(other.start_line),
            )
        })
        .copied()
}

/// A JS/TS `const`/`let`/`var`/`using` declaration's first line, after any
/// `export`/`declare`/`await` modifiers (`await using f = …`).
fn is_declarator_form(signature: &str) -> bool {
    signature
        .split_whitespace()
        .find(|word| !matches!(*word, "export" | "declare" | "await"))
        .is_some_and(|word| matches!(word, "const" | "let" | "var" | "using"))
}

fn name_is_locally_bound(
    bindings: &[&RawReference],
    scope_index: &FileScopeIndex<'_>,
    sorted_syms: &[&RawSymbol],
    reference: &RawReference,
    language: Language,
) -> bool {
    if reference.scope.is_some() {
        let name = reference
            .receiver
            .as_deref()
            .and_then(|receiver| receiver.split('.').next())
            .unwrap_or(&reference.name);
        return active_lexical_binding(scope_index, reference, name, language).is_some_and(
            |binding| {
                !binding.scope.is_some_and(|scope| {
                    scope.initialized_at > scope.position
                        && reference.scope.is_some_and(|call| {
                            call.position >= scope.initialized_at
                                // A nested function body may close over a
                                // later callable declaration in its outer scope.
                                // A direct use in the declaration's own body
                                // retains the temporal-dead-zone guard.
                                || call.start > scope.start && call.end <= scope.end
                        })
                        && sorted_syms.iter().any(|symbol| {
                            symbol.name == reference.name && symbol.start_line == binding.start_line
                        })
                })
            },
        );
    }
    let enclosing = match find_enclosing_symbol(sorted_syms, reference.start_line) {
        Enclosing::Exact(symbol) | Enclosing::Degenerate(symbol) => symbol,
        Enclosing::ModuleScope => return false,
    };
    bindings.iter().any(|binding| {
        let name = binding.name.as_str();
        let line = binding.start_line;
        (name == reference.name
            || reference
                .receiver
                .as_deref()
                .is_some_and(|receiver| receiver.split('.').next() == Some(name)))
            && line <= reference.start_line
            && line >= enclosing.start_line
            && line <= enclosing.end_line
            && !sorted_syms
                .iter()
                .any(|symbol| symbol.name == reference.name && symbol.start_line == line)
    })
}

fn unresolved_edge(source_uid: String, name: &str, edge_type: EdgeType) -> ResolvedEdge {
    ResolvedEdge {
        source_uid,
        target_uid: format!("unresolved:{name}"),
        edge_type,
        confidence: 0.0,
        link_type: None,
        evidence: vec![EdgeEvidence {
            kind: "unresolved".to_string(),
            weight: 0.0,
            note: None,
        }],
    }
}

/// How many re-export hops the Priority 3 tier walks.
///
/// nw-323: one hop was not enough for a real TypeScript barrel
/// (`common/errors.ts -> errors/index.ts -> http-errors.ts` is two), and an
/// unbounded walk would reinstate exactly the fan-out that nw-103 and nw-153
/// exist to prevent. Three covers the observed chains with headroom.
const REEXPORT_MAX_HOPS: usize = 3;

/// Whether `receiver` is a PATH receiver — `HashMap`, `std::collections::HashMap`,
/// `Foo::Bar` — as opposed to a value expression.
///
/// The distinction matters because path receivers are handled by the
/// path-qualified tier and are deliberately exempt from the value-receiver gate.
///
/// Measured defect: the exemption used to be `receiver.contains("::")`, which is
/// not the same question. A chained Rust expression that merely MENTIONS a path
/// somewhere inside it — `arr.iter().filter_map(|v| v.as_str().map(String::from))`
/// — contains `::` and so escaped the gate entirely. That is how `collect`,
/// `len` and `contains` kept their in-degree after the nw-308 gate was added to
/// every tier: the gate was there, and this predicate waved them past it. A path
/// receiver is a path ALL THE WAY THROUGH, so test the whole string rather than
/// asking whether a substring occurs in it.
fn is_path_receiver(receiver: &str) -> bool {
    if !receiver.contains("::") {
        return false;
    }
    receiver.split("::").all(|segment| {
        !segment.is_empty()
            && segment
                .chars()
                .all(|c| c.is_alphanumeric() || c == '_' || c == '$')
    })
}

/// Whether `receiver` could plausibly denote a symbol declared in
/// `candidate_file`.
///
/// nw-150 established this test and nw-308/nw-327 established that it has to
/// hold at EVERY name-only tier, not just the weakest one. The original
/// comment, still below at Priority 4, says why: `knex.where(..)` is captured
/// as a call to the bare name `where`, and binding that to whatever same-named
/// symbol happens to be in scope made a block-scoped `const where = {..}` the
/// most-depended-on symbol in a 193k-symbol graph. The identical failure at the
/// import tier made `collect`, `contains`, `is_empty`, `len` and `path` the
/// "architectural core" of a 44-repo graph — a measure of import fan-in over
/// generic vocabulary, not of architecture.
///
/// A reference with NO receiver (a plain function call, `find_hub_nodes()`) is
/// waved through unchanged: an import is the only evidence available for those,
/// they are the majority of real edges, and gating them would be a large
/// recall regression for no precision gain.
///
/// A receiver is accepted when its last segment names either the candidate's
/// FILE (`self.store.query()` -> `store` -> `store.rs`) or the candidate's
/// DECLARING TYPE (`Logger.write()` -> a method whose `parent_name` is
/// `Logger`). A path receiver containing `::` is excluded here because the
/// path-qualified tier above already handles it.
fn receiver_denotes(candidate_file: &str, sym: &RawSymbol, receiver: Option<&str>) -> bool {
    let Some(receiver) = receiver else {
        return true;
    };
    let Some(denoted) = receiver
        .rsplit(['.', ':'])
        .find(|segment| !segment.is_empty())
    else {
        return false;
    };
    let stem = candidate_file
        .rsplit('/')
        .next()
        .and_then(|base| base.split('.').next());
    if stem == Some(denoted) && (!candidate_file.ends_with(".swift") || sym.parent_name.is_none()) {
        return true;
    }
    sym.parent_name.as_deref() == Some(denoted)
}

/// What is actually known about the source of a reference.
///
/// nw-349. `find_enclosing_symbol` used to return `Option<&RawSymbol>`, which
/// collapsed THREE distinct facts into two values:
///
/// - a real span contains this line;
/// - no span contains it, but a preceding one-line symbol is the best guess;
/// - this line is module-level code and belongs to no symbol.
///
/// The caller could not act on the difference because the difference was not in
/// the type. `resolve_single_reference`'s `?` silently discarded the third case
/// — so a shell script's own `main "$@"` produced no edge at all and `dead-code`
/// reported the script's entry point as unreachable — while trusting the second
/// case as if it were the first.
#[derive(Debug, Clone, Copy)]
enum Enclosing<'a> {
    /// A symbol's real span contains `ref_line`.
    Exact(&'a RawSymbol),
    /// No span contains `ref_line`, but a preceding code-bearing symbol has a
    /// degenerate (zero-height) span, so it is the best available guess. A
    /// GUESS, and the type says so.
    Degenerate(&'a RawSymbol),
    /// `ref_line` belongs to no symbol: module-level code. A fact, not a
    /// failure to resolve.
    ModuleScope,
}

impl<'a> Enclosing<'a> {
    /// The symbol, for callers that only need "some enclosing symbol" and have
    /// no use for the distinction.
    fn symbol(self) -> Option<&'a RawSymbol> {
        match self {
            Enclosing::Exact(s) | Enclosing::Degenerate(s) => Some(s),
            Enclosing::ModuleScope => None,
        }
    }
}

/// Whether a symbol of this kind can contain executable code, and therefore be
/// the source of a call, type reference or field access.
///
/// The degenerate fallback walks backwards to the nearest one-line symbol, and
/// with no kind restriction that was frequently a DATA symbol. Measured on the
/// checked-in fixtures before this restriction existed:
///
/// - `testdata/cpp/simple.cpp:35` recorded `logValue(temp)` as **the local
///   variable `temp`** calling `logValue`;
/// - `testdata/python/simple.py:37` recorded `main()` as **`Property name`**;
/// - `testdata/js/simple.js:28` recorded `greet()` as **`Constant dog`**.
///
/// Those are not missing edges — they are edges with fabricated sources, which
/// is worse, because a caller cannot tell them from real ones. A `Constant`,
/// `Variable`, `Property` or `Field` has no body and cannot call anything.
///
/// The rule itself lives on `SymbolKind::contains_code` so the parser's
/// `<module>` minting gate applies the same definition (nw-113).
///
/// nw-113: the per-file `<module>` owner is code-bearing by kind but is never
/// a container. Its whole-file span would otherwise enclose every line, make
/// every declaration look nested, and own every module-level reference. It is
/// only the CALLS source for module-level calls (see
/// `resolve_single_reference`), so every container test goes through here.
///
/// A Rust `mod x;` declaration is a `Module` with no body: its code lives in
/// another file. As a one-line symbol it used to win the degenerate fallback
/// for every following module-level line, so `use util::double;` written
/// under `mod util;` was recorded as the module `util` importing `double`,
/// and `blast-radius` on `double` listed the module as affected.
fn can_contain_code(symbol: &RawSymbol) -> bool {
    symbol.kind.contains_code() && !symbol.is_module_owner() && !is_bodiless_module(symbol)
}

/// A module declaration whose body lives elsewhere (`mod x;`).
fn is_bodiless_module(symbol: &RawSymbol) -> bool {
    symbol.kind == SymbolKind::Module
        && symbol.end_line <= symbol.start_line
        && symbol.signature.trim_end().ends_with(';')
}

/// nw-113: the symbol whose body scopes a binding declared on `line`, or
/// `None` for module scope. Only an exact span scopes. JS/TS/Python/PHP spans
/// are exact (the languages that mint a `<module>` owner), so a degenerate
/// guess there is module-level code: it reads as module scope, mirroring how
/// `resolve_single_reference` maps a degenerate call source to the owner. In
/// other languages the guess stands, matching the source it is compared to.
fn scoping_owner<'a>(
    sorted_syms: &'a [&'a RawSymbol],
    line: u32,
    language: Language,
) -> Option<&'a RawSymbol> {
    match find_enclosing_symbol(sorted_syms, line) {
        Enclosing::Exact(symbol) => Some(symbol),
        Enclosing::Degenerate(_) if has_exact_spans(language) => None,
        Enclosing::Degenerate(symbol) => Some(symbol),
        Enclosing::ModuleScope => None,
    }
}

/// The languages whose code-bearing spans always cover their bodies, so a
/// line outside every span is module-level code (and may get a `<module>`
/// owner).
fn has_exact_spans(language: Language) -> bool {
    matches!(
        language,
        Language::JavaScript | Language::TypeScript | Language::Python | Language::Php
    )
}

/// Find the enclosing symbol: the innermost symbol whose span contains the
/// reference line (`start_line <= ref_line <= end_line`).
///
/// Requires `symbols` to be sorted by `start_line` ascending (tree-sitter LR-parser guarantee).
/// Binary search finds the last symbol starting at or before `ref_line`; if that
/// symbol's span does not contain the line (e.g. Python module-level statements
/// like `if __name__ == '__main__':` after the last `def`), walk back to an
/// earlier symbol whose span does — or report `ModuleScope` when the reference
/// is module-level code that belongs to no symbol.
///
/// nw-435. The exact-match search below used to accept ANY symbol whose span
/// contained `ref_line`, with no kind check — unlike the degenerate-span
/// fallback, which nw-349 already restricted to `can_contain_code`. A
/// module-level `name = call(...)` mints a one-line `Variable`/`Constant` for
/// the assignment target, and that symbol's degenerate span (`start_line ==
/// end_line == ref_line`) satisfies the exact-match condition on the SAME
/// line as the call it sits next to. So the call resolved into the
/// assignment target instead of module scope: measured on a real corpus,
/// `lbug_version = _get_lbug_version()` in `scripts/pip-package/setup.py`
/// recorded `_get_lbug_version`'s caller as the `Variable lbug_version`. A
/// `Variable`/`Constant`/`Property`/`Field` has no body and cannot be a call
/// site — that is a fabricated source, the same shape nw-349 already fixed
/// for the degenerate fallback, just reached through the exact-match branch
/// instead. The exact-match search now carries the identical restriction, so
/// both branches treat "no body" the same way regardless of which one found
/// the candidate span.
///
/// Degenerate-span fallback: some parsers still emit one-line spans while
/// emitting Call references on later lines, so no span can contain those refs.
/// When no span contains the line, attribute the reference to the nearest
/// preceding CODE-BEARING symbol with a degenerate span (`end_line <=
/// start_line`) and report it as `Degenerate`. Symbols with real spans that
/// ended before `ref_line` still yield `ModuleScope` — module-level code
/// belongs to no symbol.
fn find_enclosing_symbol<'a>(symbols: &'a [&'a RawSymbol], ref_line: u32) -> Enclosing<'a> {
    debug_assert!(
        symbols
            .windows(2)
            .all(|w| w[0].start_line <= w[1].start_line),
        "find_enclosing_symbol requires symbols sorted by start_line"
    );
    if symbols.is_empty() {
        return Enclosing::ModuleScope;
    }
    let idx = symbols.partition_point(|s| s.start_line <= ref_line);
    if idx == 0 {
        return Enclosing::ModuleScope;
    }
    if let Some(enclosing) = symbols[..idx]
        .iter()
        .rev()
        .find(|s| ref_line <= s.end_line.max(s.start_line) && can_contain_code(s))
        .copied()
    {
        return Enclosing::Exact(enclosing);
    }
    symbols[..idx]
        .iter()
        .rev()
        .find(|s| s.end_line <= s.start_line && can_contain_code(s))
        .copied()
        .map_or(Enclosing::ModuleScope, Enclosing::Degenerate)
}

#[cfg(test)]
mod tests {
    use super::*;
    use nestweaver_parser::ReferenceKind;
    use nestweaver_schema::{SymbolKind, Visibility};

    fn make_symbol(name: &str, line: u32) -> RawSymbol {
        RawSymbol {
            name: name.to_string(),
            kind: SymbolKind::Function,
            start_line: line,
            // Real parser spans cover the body; keep fixtures realistic so the
            // enclosing-symbol end_line check behaves like production.
            end_line: line + 5,
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

    fn make_ref(name: &str, kind: ReferenceKind, line: u32) -> RawReference {
        RawReference {
            scope: None,
            name: name.to_string(),
            kind,
            start_line: line,
            context: String::new(),
            receiver: None,
        }
    }

    #[test]
    fn scoped_call_index_reads_each_reference_once_and_preserves_ambiguity() {
        use nestweaver_parser::LexicalScope;
        let scope = |position| LexicalScope {
            position,
            start: 0,
            end: 10000,
            initialized_at: 0,
            hoisted_var: false,
        };
        let mut references: Vec<_> = (0..2000)
            .map(|i| {
                let mut reference = make_ref("other", ReferenceKind::Call, 1);
                reference.scope = Some(scope(i));
                reference
            })
            .collect();
        let mut call = make_ref("factory", ReferenceKind::Call, 2);
        call.scope = Some(scope(3000));
        references.push(call.clone());
        references.push(call.clone());
        call.receiver = Some("value".into());
        references.push(call.clone());
        call.scope = Some(scope(3001));
        references.push(call.clone());
        call.kind = ReferenceKind::LocalBinding;
        references.push(call.clone());
        call.kind = ReferenceKind::Call;
        call.scope = None;
        references.push(call);
        let reads = std::cell::Cell::new(0);
        let index = scoped_call_reference_index(references.iter().inspect(|_| {
            reads.set(reads.get() + 1);
        }));
        assert_eq!(reads.get(), references.len());
        for _ in 0..32 {
            assert_eq!(
                index.get(&("factory", None, 3000)).unwrap().len(),
                2,
                "duplicate exact call references must stay ambiguous"
            );
            assert_eq!(
                index.get(&("factory", Some("value"), 3000)).unwrap().len(),
                1
            );
            assert_eq!(
                index.get(&("factory", Some("value"), 3001)).unwrap().len(),
                1,
                "binding references and unscoped calls cannot donate identity"
            );
        }
        assert_eq!(
            reads.get(),
            references.len(),
            "assignment probes never rescan unrelated calls"
        );
        assert_eq!(index.len(), 2003);
    }

    fn make_binding(local: &str, original: &str, specifier: &str, line: u32) -> RawReference {
        RawReference {
            scope: None,
            name: local.into(),
            kind: ReferenceKind::ImportAlias,
            start_line: line,
            context: specifier.into(),
            receiver: Some(original.into()),
        }
    }

    fn make_export(public: &str, local: &str, source: Option<&str>, line: u32) -> RawReference {
        RawReference {
            scope: None,
            name: public.into(),
            kind: ReferenceKind::ExportAlias,
            start_line: line,
            context: local.into(),
            receiver: source.map(str::to_string),
        }
    }

    #[test]
    fn resolves_same_file_call() {
        // caller() at line 10 calls helper() at line 1, both in same file
        let files = vec![(
            "src/main.js".to_string(),
            vec![make_symbol("helper", 1), make_symbol("caller", 10)],
            vec![make_ref("helper", ReferenceKind::Call, 12)],
        )];

        let edges = resolve_references(&files, Language::JavaScript, "repo:test:abc");
        assert!(!edges.is_empty(), "should produce at least one edge");
        let edge = &edges[0];
        let expected_confidence = confidence_score(MatchType::SameFileExact, Language::JavaScript);
        assert!(
            (edge.confidence - expected_confidence).abs() < f32::EPSILON,
            "expected same-file confidence {expected_confidence}, got {}",
            edge.confidence
        );
        assert!(
            !edge.target_uid.starts_with("unresolved:"),
            "should not be unresolved"
        );
    }

    #[test]
    fn resolves_imported_symbol() {
        // main.js imports helper.js and calls helperFn
        let files = vec![
            (
                "src/main.js".to_string(),
                vec![make_symbol("main", 5)],
                vec![
                    make_ref("./helper", ReferenceKind::Import, 1),
                    make_binding("helperFn", "helperFn", "./helper", 1),
                    make_ref("helperFn", ReferenceKind::Call, 10),
                ],
            ),
            (
                "src/helper.js".to_string(),
                vec![make_symbol("helperFn", 1)],
                vec![make_export("helperFn", "helperFn", None, 1)],
            ),
        ];

        let edges = resolve_references(&files, Language::JavaScript, "repo:test:abc");
        let call_edges: Vec<_> = edges
            .iter()
            .filter(|e| e.edge_type == EdgeType::Calls)
            .collect();
        assert!(
            !call_edges.is_empty(),
            "should produce call edges; all edges: {edges:?}"
        );

        let edge = &call_edges[0];
        let expected_confidence = confidence_score(MatchType::ImportResolved, Language::JavaScript);
        assert!(
            (edge.confidence - expected_confidence).abs() < f32::EPSILON,
            "expected import-resolved confidence {expected_confidence}, got {}",
            edge.confidence
        );
        assert!(
            !edge.target_uid.starts_with("unresolved:"),
            "should not be unresolved"
        );
    }

    #[test]
    fn unresolved_reference_gets_zero_confidence() {
        let files = vec![(
            "src/main.js".to_string(),
            vec![make_symbol("caller", 1)],
            vec![make_ref("unknownFn", ReferenceKind::Call, 5)],
        )];

        let edges = resolve_references(&files, Language::JavaScript, "repo:test:abc");
        assert!(!edges.is_empty());
        let edge = &edges[0];
        assert!(
            (edge.confidence - 0.0).abs() < f32::EPSILON,
            "unresolved should have 0.0 confidence, got {}",
            edge.confidence
        );
        assert!(
            edge.target_uid.starts_with("unresolved:"),
            "target_uid should start with 'unresolved:', got {}",
            edge.target_uid
        );
    }

    /// nw-724: `items.len()` does not call a free `fn len`, in this file or
    /// in a file this file imports.
    #[test]
    fn a_method_call_does_not_resolve_to_a_free_function_of_the_same_name() {
        let mut caller = make_symbol("check", 10);
        caller.end_line = 30;
        let mut call = make_ref("len", ReferenceKind::Call, 20);
        call.receiver = Some("items".to_string());
        let mut same_file = make_ref("len", ReferenceKind::Call, 12);
        same_file.receiver = Some("items".to_string());

        let files = vec![
            (
                "src/check.rs".to_string(),
                vec![make_symbol("len", 1), caller],
                vec![
                    make_ref("./regex_index", ReferenceKind::Import, 1),
                    same_file,
                    call.clone(),
                ],
            ),
            (
                "src/regex_index.rs".to_string(),
                vec![make_symbol("len", 186)],
                vec![],
            ),
        ];
        let edges = resolve_references(&files, Language::Rust, "repo:test:abc");
        let resolved: Vec<_> = edges
            .iter()
            .filter(|edge| {
                edge.edge_type == EdgeType::Calls && !edge.target_uid.starts_with("unresolved:")
            })
            .collect();
        assert!(
            resolved.is_empty(),
            "items.len() must not resolve to fn len: {resolved:?}"
        );
    }

    /// nw-724: `format!(...)` does not call a field named `format`, and
    /// `Err(...)` does not call `type Err = String`.
    #[test]
    fn a_macro_and_a_type_alias_are_not_call_targets() {
        let mut format_field = make_symbol("format", 87);
        format_field.kind = SymbolKind::Property;
        let mut err_alias = make_symbol("Err", 4);
        err_alias.kind = SymbolKind::TypeAlias;
        let mut caller = make_symbol("show", 10);
        caller.end_line = 40;
        let files = vec![(
            "src/engine_format.rs".to_string(),
            vec![format_field, err_alias, caller],
            vec![
                make_ref("format", ReferenceKind::Macro, 20),
                make_ref("Err", ReferenceKind::Call, 22),
            ],
        )];
        let edges = resolve_references(&files, Language::Rust, "repo:test:abc");
        let resolved: Vec<_> = edges
            .iter()
            .filter(|edge| {
                edge.edge_type == EdgeType::Calls && !edge.target_uid.starts_with("unresolved:")
            })
            .collect();
        assert!(
            resolved.is_empty(),
            "format! and Err(...) must not resolve to the field or the alias: {resolved:?}"
        );
    }

    /// nw-724: a call of a locally bound name does not resolve to a function
    /// of that name.
    #[test]
    fn a_locally_bound_name_does_not_resolve_to_another_function() {
        let mut caller = make_symbol("run", 10);
        caller.end_line = 20;
        let files = vec![(
            "src/run.kt".to_string(),
            vec![make_symbol("transform", 1), caller],
            vec![
                make_ref("transform", ReferenceKind::LocalBinding, 10),
                make_ref("transform", ReferenceKind::Call, 12),
            ],
        )];
        let edges = resolve_references(&files, Language::Kotlin, "repo:test:abc");
        let phantom = symbol_uid("repo:test:abc", "src/run.kt", "transform", 1);
        assert!(
            !edges.iter().any(|edge| edge.target_uid == phantom),
            "a lambda parameter must not call the function of the same name: {edges:?}"
        );
    }

    /// nw-150: a method call must not bind to an unrelated same-named symbol
    /// just because it sits in a sibling file.
    ///
    /// Real case: `knex.where({..})` in a test file was captured as a call to
    /// the bare name `where` and bound to `const where = {..}` -- a block-local
    /// inside an else-branch of an unrelated resolver. That made it the single
    /// most-depended-on symbol in a 193k-symbol graph (in_degree 1048, 524
    /// bogus CALLS dependents) and poisoned hubs, bridges and PageRank.
    #[test]
    fn a_method_call_does_not_bind_to_an_unrelated_same_named_symbol() {
        let mut caller = make_symbol("checkin_test", 10);
        caller.end_line = 40;
        let mut call = make_ref("where", ReferenceKind::Call, 20);
        call.receiver = Some("knex".to_string());

        let files = vec![
            ("src/checkin.test.js".to_string(), vec![caller], vec![call]),
            // Sibling file declaring a same-named symbol it has nothing to do with.
            (
                "src/setVideoViewStatus.js".to_string(),
                vec![make_symbol("where", 84)],
                vec![],
            ),
        ];
        let edges = resolve_references(&files, Language::JavaScript, "repo:test:abc");
        let bogus: Vec<_> = edges
            .iter()
            .filter(|e| e.edge_type == EdgeType::Calls && !e.target_uid.starts_with("unresolved:"))
            .collect();
        assert!(
            bogus.is_empty(),
            "knex.where() must not resolve to an unrelated local: {bogus:?}"
        );
    }

    /// The gate must still allow a receiver that genuinely denotes the file.
    #[test]
    fn a_method_call_still_resolves_when_the_receiver_names_the_file() {
        let mut caller = make_symbol("handler", 10);
        caller.end_line = 40;
        let mut call = make_ref("connect", ReferenceKind::Call, 20);
        call.receiver = Some("database".to_string());

        let files = vec![
            ("src/handler.js".to_string(), vec![caller], vec![call]),
            (
                "src/database.js".to_string(),
                vec![make_symbol("connect", 5)],
                vec![],
            ),
        ];
        let edges = resolve_references(&files, Language::JavaScript, "repo:test:abc");
        let expected = symbol_uid("repo:test:abc", "src/database.js", "connect", 5);
        assert!(
            edges
                .iter()
                .any(|e| e.edge_type == EdgeType::Calls && e.target_uid == expected),
            "database.connect() should still resolve to database.js"
        );
    }

    /// nw-327 / nw-308: the nw-150 receiver gate was applied to the
    /// same-package fallback ONLY. Priority 2 (direct imports) returns first
    /// and had no gate, so importing ANY symbol from a file donated every bare
    /// method name in it.
    ///
    /// Real case: `crates/nestweaver-mcp/src/tools.rs:30` imports `SearchTotal`
    /// from `tantivy_index.rs`; `.collect()` at :9431 then bound to the private
    /// `SegmentCollector::collect` at `tantivy_index.rs:202` at ImportResolved
    /// confidence, giving a function with zero real callers 498 in-edges.
    #[test]
    fn an_imported_file_does_not_donate_its_method_names_to_bare_calls() {
        let mut caller = make_symbol("tool_hub_nodes", 10);
        caller.end_line = 60;
        let mut call = make_ref("collect", ReferenceKind::Call, 40);
        // The receiver of a chained `.collect()` is the whole preceding chain.
        call.receiver = Some("hubs.iter().map(|h| render(h))".to_string());

        let files = vec![
            (
                "src/tools.js".to_string(),
                vec![caller],
                vec![
                    // The file is imported for an UNRELATED symbol.
                    make_ref("./tantivy_index", ReferenceKind::Import, 1),
                    call,
                ],
            ),
            (
                "src/tantivy_index.js".to_string(),
                vec![make_symbol("SearchTotal", 5), make_symbol("collect", 202)],
                vec![],
            ),
        ];

        let edges = resolve_references(&files, Language::JavaScript, "repo:test:abc");
        let phantom = symbol_uid("repo:test:abc", "src/tantivy_index.js", "collect", 202);
        assert!(
            !edges
                .iter()
                .any(|e| e.edge_type == EdgeType::Calls && e.target_uid == phantom),
            "a chained .collect() must not bind to an unrelated `collect` merely \
             because the file was imported for something else: {edges:?}"
        );
    }

    /// The `::` exemption must test whether the receiver IS a path, not whether
    /// it CONTAINS one. Measured on this repo: with `contains("::")`,
    /// `collect` kept an in-degree of 771 and `len` 673 even with the gate
    /// applied to every tier, because a chained Rust expression mentioning
    /// `String::from` was classified as a path receiver and waved through.
    #[test]
    fn a_chain_mentioning_a_path_is_still_a_value_receiver() {
        assert!(is_path_receiver("HashMap::new"));
        assert!(is_path_receiver("std::collections::HashMap"));
        assert!(!is_path_receiver("store"));
        assert!(!is_path_receiver(
            "arr.iter().filter_map(|v| v.as_str().map(String::from))"
        ));
        assert!(!is_path_receiver("self.store.query()"));
        assert!(!is_path_receiver("xs.iter().map(Vec::new)"));
    }

    /// End to end: the real shape that survived the first cut of the gate.
    #[test]
    fn a_chained_call_mentioning_a_path_does_not_donate_a_method_name() {
        let mut caller = make_symbol("extract_string_array", 10);
        caller.end_line = 60;
        let mut call = make_ref("collect", ReferenceKind::Call, 40);
        call.receiver = Some("arr.iter().filter_map(|v| v.as_str().map(String::from))".to_string());

        let files = vec![
            (
                "src/tools.js".to_string(),
                vec![caller],
                vec![make_ref("./tantivy_index", ReferenceKind::Import, 1), call],
            ),
            (
                "src/tantivy_index.js".to_string(),
                vec![make_symbol("SearchTotal", 5), make_symbol("collect", 202)],
                vec![],
            ),
        ];
        let edges = resolve_references(&files, Language::JavaScript, "repo:test:abc");
        let phantom = symbol_uid("repo:test:abc", "src/tantivy_index.js", "collect", 202);
        assert!(
            !edges
                .iter()
                .any(|e| e.edge_type == EdgeType::Calls && e.target_uid == phantom),
            "a chain that merely MENTIONS `String::from` is a value receiver \
             and must be gated: {edges:?}"
        );
    }

    /// Guard rail: a genuine path receiver must still reach the qualified tier.
    #[test]
    fn a_genuine_path_receiver_still_resolves() {
        let mut caller = make_symbol("main", 5);
        caller.end_line = 20;
        let mut call = make_ref("new", ReferenceKind::Call, 10);
        call.receiver = Some("store::GraphStore".to_string());
        let files = vec![
            (
                "src/main.rs".to_string(),
                vec![caller],
                vec![make_ref("./store", ReferenceKind::Import, 1), call],
            ),
            (
                "src/store.rs".to_string(),
                vec![make_symbol("new", 5)],
                vec![],
            ),
        ];
        let edges = resolve_references(&files, Language::Rust, "repo:test:abc");
        let expected = symbol_uid("repo:test:abc", "src/store.rs", "new", 5);
        assert!(
            edges
                .iter()
                .any(|e| e.edge_type == EdgeType::Calls && e.target_uid == expected),
            "a real path receiver must still resolve: {edges:?}"
        );
    }

    /// Where else does this property hold? Priority 3 (re-exports) has the
    /// identical shape and the identical omission — one hop further out.
    #[test]
    fn a_reexported_file_does_not_donate_its_method_names_to_bare_calls() {
        let mut caller = make_symbol("tool_hub_nodes", 10);
        caller.end_line = 60;
        let mut call = make_ref("collect", ReferenceKind::Call, 40);
        call.receiver = Some("hubs.iter()".to_string());

        let files = vec![
            (
                "src/tools.js".to_string(),
                vec![caller],
                vec![make_ref("./barrel", ReferenceKind::Import, 1), call],
            ),
            (
                "src/barrel.js".to_string(),
                vec![],
                vec![make_ref("./tantivy_index", ReferenceKind::Import, 1)],
            ),
            (
                "src/tantivy_index.js".to_string(),
                vec![make_symbol("collect", 202)],
                vec![],
            ),
        ];

        let edges = resolve_references(&files, Language::JavaScript, "repo:test:abc");
        let phantom = symbol_uid("repo:test:abc", "src/tantivy_index.js", "collect", 202);
        assert!(
            !edges
                .iter()
                .any(|e| e.edge_type == EdgeType::Calls && e.target_uid == phantom),
            "the re-export tier must carry the same receiver gate: {edges:?}"
        );
    }

    /// The Priority-2 gate must not touch plain function calls: those carry no
    /// receiver and an import is the only evidence available for them.
    #[test]
    fn a_receiverless_call_still_resolves_through_a_direct_import() {
        let mut caller = make_symbol("main", 5);
        caller.end_line = 20;
        let files = vec![
            (
                "src/main.js".to_string(),
                vec![caller],
                vec![
                    make_ref("./helper", ReferenceKind::Import, 1),
                    make_binding("helperFn", "helperFn", "./helper", 1),
                    make_ref("helperFn", ReferenceKind::Call, 10), // receiver: None
                ],
            ),
            (
                "src/helper.js".to_string(),
                vec![make_symbol("helperFn", 1)],
                vec![make_export("helperFn", "helperFn", None, 1)],
            ),
        ];
        let edges = resolve_references(&files, Language::JavaScript, "repo:test:abc");
        let expected = symbol_uid("repo:test:abc", "src/helper.js", "helperFn", 1);
        assert!(
            edges
                .iter()
                .any(|e| e.edge_type == EdgeType::Calls && e.target_uid == expected),
            "receiver-less import-resolved calls must keep resolving"
        );
    }

    /// A receiver that genuinely denotes the imported file must still bind
    /// through Priority 2 — the gate is a discriminator, not a ban.
    #[test]
    fn a_denoting_receiver_still_resolves_through_a_direct_import() {
        let mut caller = make_symbol("handler", 10);
        caller.end_line = 40;
        let mut call = make_ref("query", ReferenceKind::Call, 20);
        call.receiver = Some("store".to_string());

        let files = vec![
            (
                "src/handler.js".to_string(),
                vec![caller],
                vec![
                    make_ref("./store", ReferenceKind::Import, 1),
                    make_binding("store", "*", "./store", 1),
                    call,
                ],
            ),
            (
                "src/store.js".to_string(),
                vec![make_symbol("query", 5)],
                vec![make_export("query", "query", None, 5)],
            ),
        ];
        let edges = resolve_references(&files, Language::JavaScript, "repo:test:abc");
        let expected = symbol_uid("repo:test:abc", "src/store.js", "query", 5);
        assert!(
            edges
                .iter()
                .any(|e| e.edge_type == EdgeType::Calls && e.target_uid == expected),
            "store.query() must still resolve to the imported store.js: {edges:?}"
        );
    }

    /// nw-323 / nw-324, end to end: the acceptance test for defect B.
    #[test]
    fn js_specifier_across_directories_produces_a_resolved_call_edge() {
        // src/common/__tests__/money.test.ts imports '../money.js' (a .ts file)
        // and calls roundMoney. The two files are in DIFFERENT directories, so
        // the same-package fallback cannot rescue it: this was `unresolved:`.
        let files = vec![
            (
                "src/common/money.ts".to_string(),
                vec![make_symbol("roundMoney", 5)],
                vec![make_export("roundMoney", "roundMoney", None, 5)],
            ),
            (
                "src/common/__tests__/money.test.ts".to_string(),
                vec![make_symbol("rounds to 2 decimal places", 6)],
                vec![
                    make_ref("../money.js", ReferenceKind::Import, 3),
                    make_binding("roundMoney", "roundMoney", "../money.js", 3),
                    make_ref("roundMoney", ReferenceKind::Call, 7),
                ],
            ),
        ];

        let edges = resolve_references(&files, Language::TypeScript, "repo:test:abc");

        let call = edges
            .iter()
            .find(|e| e.edge_type == EdgeType::Calls)
            .expect("a CALLS edge must exist");
        assert!(
            !call.target_uid.starts_with("unresolved:"),
            "nw-323/nw-324: `../money.js` must resolve to money.ts; got {}",
            call.target_uid
        );
        let expected = confidence_score(MatchType::ImportResolved, Language::TypeScript);
        assert!(
            (call.confidence - expected).abs() < f32::EPSILON,
            "must be import-resolved ({expected}), not the 0.50 same-package guess; got {}",
            call.confidence
        );
        assert!(
            edges.iter().any(|e| e.edge_type == EdgeType::Imports),
            "the actual binding user must also have an IMPORTS dependency"
        );
    }

    /// nw-323 defect C, second half: the re-export tier walked exactly ONE hop,
    /// but a real barrel chain is `errors.ts -> errors/index.ts ->
    /// http-errors.ts` — two hops.
    #[test]
    fn a_two_hop_barrel_chain_resolves() {
        let mut caller = make_symbol("get", 10);
        caller.end_line = 20;
        let files = vec![
            (
                "src/modules/a/service.ts".to_string(),
                vec![caller],
                vec![
                    make_ref("../../common/errors.js", ReferenceKind::Import, 1),
                    make_binding(
                        "NotFoundError",
                        "NotFoundError",
                        "../../common/errors.js",
                        1,
                    ),
                    make_ref("NotFoundError", ReferenceKind::Call, 12),
                ],
            ),
            (
                "src/common/errors.ts".to_string(),
                vec![],
                vec![
                    make_ref("./errors/index.js", ReferenceKind::Import, 1),
                    make_export(
                        "NotFoundError",
                        "NotFoundError",
                        Some("./errors/index.js"),
                        1,
                    ),
                ],
            ),
            (
                "src/common/errors/index.ts".to_string(),
                vec![],
                vec![
                    make_ref("./http-errors.js", ReferenceKind::Import, 1),
                    make_export(
                        "NotFoundError",
                        "NotFoundError",
                        Some("./http-errors.js"),
                        1,
                    ),
                ],
            ),
            (
                "src/common/errors/http-errors.ts".to_string(),
                vec![make_symbol("NotFoundError", 20)],
                vec![make_export("NotFoundError", "NotFoundError", None, 20)],
            ),
        ];
        let edges = resolve_references(&files, Language::TypeScript, "repo:test:abc");
        let expected = symbol_uid(
            "repo:test:abc",
            "src/common/errors/http-errors.ts",
            "NotFoundError",
            20,
        );
        assert!(
            edges
                .iter()
                .any(|e| e.edge_type == EdgeType::Calls && e.target_uid == expected),
            "a two-hop barrel chain must resolve: {edges:?}"
        );
    }

    #[test]
    fn python_gets_lower_confidence_than_java() {
        // Both have an import-resolved call, but Python confidence < Java confidence
        let make_files = |file: &str, imp: &str, target_file: &str| {
            vec![
                (
                    file.to_string(),
                    vec![make_symbol("caller", 5)],
                    vec![
                        make_ref(imp, ReferenceKind::Import, 1),
                        make_ref("targetFn", ReferenceKind::Call, 10),
                    ],
                ),
                (
                    target_file.to_string(),
                    vec![make_symbol("targetFn", 1)],
                    vec![],
                ),
            ]
        };

        let java_files = make_files(
            "com/example/Main.java",
            "com.example.Helper",
            "com/example/Helper.java",
        );
        let python_files = make_files("app/main.py", ".helper", "app/helper.py");

        let java_edges = resolve_references(&java_files, Language::Java, "repo:test:abc");
        let python_edges = resolve_references(&python_files, Language::Python, "repo:test:abc");

        let java_call = java_edges
            .iter()
            .find(|e| e.edge_type == EdgeType::Calls && !e.target_uid.starts_with("unresolved:"));
        let python_call = python_edges
            .iter()
            .find(|e| e.edge_type == EdgeType::Calls && !e.target_uid.starts_with("unresolved:"));

        assert!(java_call.is_some(), "java should have resolved call edge");
        assert!(
            python_call.is_some(),
            "python should have resolved call edge"
        );

        let java_conf = java_call.unwrap().confidence;
        let python_conf = python_call.unwrap().confidence;
        assert!(
            python_conf < java_conf,
            "python ({python_conf}) should be less than java ({java_conf})"
        );
    }

    /// nw-152: a fully-qualified call with no matching `use` must still
    /// resolve. Real case: src/main.rs calls
    /// `nestweaver_engine::publication::read_current(..)` with no `use` for
    /// that module, so the edge was dropped and `impact read_current` reported
    /// zero callers in main.rs -- while a sibling call to a DIFFERENT module
    /// resolved fine purely because an unrelated type from it was imported.
    #[test]
    fn a_fully_qualified_call_resolves_without_a_matching_use() {
        let mut caller = make_symbol("run_publication_rebuild", 10);
        caller.end_line = 40;
        let mut call = make_ref("read_current", ReferenceKind::Call, 20);
        // The parser records the qualifying path as the receiver.
        call.receiver = Some("nestweaver_engine::publication".to_string());

        let files = vec![
            (
                "src/lib.rs".to_string(),
                vec![make_symbol("root", 1)],
                vec![],
            ),
            ("src/main.rs".to_string(), vec![caller], vec![call]),
            (
                "src/publication.rs".to_string(),
                vec![make_symbol("read_current", 5)],
                vec![],
            ),
            // A decoy with the same symbol name in an unrelated module: the
            // qualifier must pick publication.rs, not this one.
            (
                "src/other.rs".to_string(),
                vec![make_symbol("read_current", 5)],
                vec![],
            ),
        ];

        let edges = resolve_references(&files, Language::Rust, "repo:test:abc");
        let calls: Vec<_> = edges
            .iter()
            .filter(|e| e.edge_type == EdgeType::Calls)
            .collect();
        assert_eq!(calls.len(), 1, "expected one CALLS edge, got {calls:?}");
        let expected = symbol_uid("repo:test:abc", "src/publication.rs", "read_current", 5);
        assert_eq!(
            calls[0].target_uid, expected,
            "the qualifier must select publication.rs over the same-named decoy"
        );
        assert!(
            !calls[0].target_uid.starts_with("unresolved:"),
            "a qualified call must not fall through to unresolved"
        );
    }

    /// The qualifier tier must NOT fire for a bare receiver: a JS
    /// `store.method()` receiver is a variable, not a module path, and
    /// matching it against a same-named file would invent edges.
    #[test]
    fn a_bare_receiver_does_not_trigger_path_qualified_resolution() {
        let mut caller = make_symbol("handler", 10);
        caller.end_line = 40;
        let mut call = make_ref("where", ReferenceKind::Call, 20);
        call.receiver = Some("knex".to_string());

        let files = vec![
            ("src/main.js".to_string(), vec![caller], vec![call]),
            (
                "src/knex.js".to_string(),
                vec![make_symbol("where", 5)],
                vec![],
            ),
        ];
        let edges = resolve_references(&files, Language::JavaScript, "repo:test:abc");
        let qualified: Vec<_> = edges
            .iter()
            .filter(|e| e.evidence.iter().any(|ev| ev.kind == "path_qualified"))
            .collect();
        assert!(
            qualified.is_empty(),
            "a bare receiver must not resolve via the path-qualified tier: {qualified:?}"
        );
    }

    #[test]
    fn no_enclosing_symbol_skips_reference() {
        // A reference at line 1 with no symbols before it
        let files = vec![(
            "src/main.js".to_string(),
            vec![make_symbol("fn", 10)], // symbol starts after reference
            vec![make_ref("something", ReferenceKind::Call, 1)],
        )];

        let edges = resolve_references(&files, Language::JavaScript, "repo:test:abc");
        // Should produce no edges since there's no enclosing symbol
        assert!(
            edges.is_empty(),
            "should skip reference with no enclosing symbol"
        );
    }

    /// A top-level import yields ONE file-level proxy edge, not one per
    /// exported symbol in the target.
    ///
    /// This test previously asserted two edges — one to every export — which is
    /// the fan-out that nw-103 removed. Attributing a top-level import to a
    /// *symbol* is a category error: the import belongs to the file, and the
    /// symbol Pass 3b picked was simply whichever happened to be declared
    /// first. That is how a never-exported string constant ended up with 830
    /// out-edges. Pass 3a keeps the file-level edge so connectivity survives.
    ///
    /// Genuine per-symbol import attribution — linking only the symbols that
    /// actually reference the imported binding — is tracked separately; it
    /// needs reference matching this pass does not do.
    ///
    /// nw-153: a `use` INSIDE a function body must resolve to the one symbol
    /// it names, not fan out to every symbol in the target file.
    ///
    /// Real case: backup_artifact_contract contains exactly one import,
    /// `use crate::publication::ArtifactKind;`, and acquired 64 IMPORTS
    /// out-edges into publication.rs -- including rollback_current and
    /// compare_and_swap_current. That is why `impact rollback_current`
    /// returned unrelated backup code while missing its real callers.
    #[test]
    fn a_named_import_inside_a_function_resolves_to_the_named_symbol_only() {
        let files = vec![
            // `crate::` resolution walks up for a crate root, so the fixture
            // needs one or the import never resolves to a file at all.
            (
                "src/lib.rs".to_string(),
                vec![make_symbol("root", 1)],
                vec![],
            ),
            (
                "src/backup.rs".to_string(),
                vec![make_symbol("backup_artifact_contract", 10)],
                vec![make_ref(
                    "crate::publication::ArtifactKind",
                    ReferenceKind::Import,
                    12,
                )],
            ),
            (
                "src/publication.rs".to_string(),
                vec![
                    make_symbol("ArtifactKind", 1),
                    make_symbol("rollback_current", 20),
                    make_symbol("compare_and_swap_current", 40),
                    make_symbol("read_current", 60),
                    make_symbol("slot_path", 80),
                ],
                vec![],
            ),
        ];

        let edges = resolve_references(&files, Language::Rust, "repo:test:abc");
        let import_edges: Vec<_> = edges
            .iter()
            .filter(|e| e.edge_type == EdgeType::Imports)
            .collect();
        assert_eq!(
            import_edges.len(),
            1,
            "one named import must yield one edge, not one per symbol in the \
             target file; got: {import_edges:?}"
        );
        // UIDs are hashed, so compare against the computed uid for the symbol
        // the specifier actually names rather than substring-matching.
        let expected = symbol_uid("repo:test:abc", "src/publication.rs", "ArtifactKind", 1);
        assert_eq!(
            import_edges[0].target_uid, expected,
            "the edge must point at the imported name, not another symbol in the file"
        );
    }

    #[test]
    fn unused_top_level_import_has_no_symbol_owner() {
        let files = vec![
            (
                "src/main.js".to_string(),
                vec![make_symbol("main", 5)],
                vec![make_ref("./helper", ReferenceKind::Import, 1)],
            ),
            (
                "src/helper.js".to_string(),
                vec![make_symbol("helperFn", 1), make_symbol("utilFn", 10)],
                vec![],
            ),
        ];

        let edges = resolve_references(&files, Language::JavaScript, "repo:test:abc");
        let import_edges: Vec<_> = edges
            .iter()
            .filter(|e| e.edge_type == EdgeType::Imports)
            .collect();
        assert!(
            import_edges.is_empty(),
            "unused module import cannot name a symbol owner: {import_edges:?}"
        );
    }

    /// nw-103: a top-level import must not turn the file's first declaration
    /// into a dependency hub.
    ///
    /// Imports sit above every declaration, so `find_enclosing_symbol` finds
    /// nothing and Pass 3b fell back to `source_symbols.first()` — then fanned
    /// out to every non-private symbol in each imported file. On the real graph
    /// that gave `const ROSTER_STORAGE_KEY = "..."` — 3 references, never
    /// exported — 830 out-edges and the #5 hub slot of a 158k-symbol graph.
    #[test]
    fn top_level_import_does_not_make_the_first_declaration_a_hub() {
        // Mirrors the real shape: a constant declared immediately after the
        // import block, then the function that actually uses the import.
        let files = vec![
            (
                "src/view.ts".to_string(),
                vec![make_symbol("STORAGE_KEY", 3), make_symbol("renderView", 20)],
                vec![make_ref("./types", ReferenceKind::Import, 1)],
            ),
            (
                "src/types.ts".to_string(),
                vec![
                    make_symbol("Alpha", 1),
                    make_symbol("Beta", 10),
                    make_symbol("Gamma", 20),
                    make_symbol("Delta", 30),
                ],
                vec![],
            ),
        ];

        let edges = resolve_references(&files, Language::TypeScript, "repo:test:abc");

        // symbol_uid hashes the name, so the UID does NOT contain the literal
        // identifier — filtering on `.contains("STORAGE_KEY")` matches nothing
        // and makes this test pass vacuously. Compute the real UID instead.
        let storage_key_uid = symbol_uid("repo:test:abc", "src/view.ts", "STORAGE_KEY", 3);
        let from_storage_key: Vec<_> = edges
            .iter()
            .filter(|e| e.edge_type == EdgeType::Imports && e.source_uid == storage_key_uid)
            .collect();

        assert!(
            from_storage_key.is_empty(),
            "STORAGE_KEY is declared after the import block and must not inherit \
             the file's imports. Got {} IMPORTS edges: {:#?}",
            from_storage_key.len(),
            from_storage_key
        );
    }

    #[test]
    fn imports_edges_skip_private_symbols() {
        // helper.js has a private symbol — should not get an IMPORTS edge
        let mut private_sym = make_symbol("_internal", 20);
        private_sym.visibility = Visibility::Private;

        let files = vec![
            (
                "src/main.js".to_string(),
                vec![make_symbol("main", 5)],
                vec![make_ref("./helper", ReferenceKind::Import, 1)],
            ),
            (
                "src/helper.js".to_string(),
                vec![make_symbol("helperFn", 1), private_sym],
                vec![],
            ),
        ];

        let edges = resolve_references(&files, Language::JavaScript, "repo:test:abc");
        let import_edges: Vec<_> = edges
            .iter()
            .filter(|e| e.edge_type == EdgeType::Imports)
            .collect();
        assert_eq!(
            import_edges.len(),
            0,
            "unused imports must not acquire a symbol owner; got: {import_edges:?}"
        );
    }

    #[test]
    fn imports_edges_coexist_with_call_edges() {
        // main.js imports helper.js AND calls helperFn — both edge types should exist
        let files = vec![
            (
                "src/main.js".to_string(),
                vec![make_symbol("main", 5)],
                vec![
                    make_ref("./helper", ReferenceKind::Import, 1),
                    make_ref("helperFn", ReferenceKind::Call, 10),
                    RawReference {
                        scope: None,
                        name: "helperFn".into(),
                        kind: ReferenceKind::ImportAlias,
                        start_line: 1,
                        context: "./helper".into(),
                        receiver: Some("helperFn".into()),
                    },
                ],
            ),
            (
                "src/helper.js".to_string(),
                vec![make_symbol("helperFn", 1)],
                vec![make_export("helperFn", "helperFn", None, 1)],
            ),
        ];

        let edges = resolve_references(&files, Language::JavaScript, "repo:test:abc");
        let call_edges: Vec<_> = edges
            .iter()
            .filter(|e| e.edge_type == EdgeType::Calls)
            .collect();
        let import_edges: Vec<_> = edges
            .iter()
            .filter(|e| e.edge_type == EdgeType::Imports)
            .collect();
        assert!(!call_edges.is_empty(), "should still produce CALLS edges");
        assert!(
            !import_edges.is_empty(),
            "should also produce IMPORTS edges"
        );
    }

    #[test]
    fn imports_edges_skip_empty_target_file() {
        // target file has no symbols — no IMPORTS edges should be created
        let files = vec![
            (
                "src/main.js".to_string(),
                vec![make_symbol("main", 5)],
                vec![make_ref("./empty", ReferenceKind::Import, 1)],
            ),
            ("src/empty.js".to_string(), vec![], vec![]),
        ];

        let edges = resolve_references(&files, Language::JavaScript, "repo:test:abc");
        let import_edges: Vec<_> = edges
            .iter()
            .filter(|e| e.edge_type == EdgeType::Imports)
            .collect();
        assert!(
            import_edges.is_empty(),
            "should not create IMPORTS edges to file with no symbols"
        );
    }

    #[test]
    fn imports_edges_use_enclosing_symbol_at_import_line() {
        // Import at line 15 — named-import pass should use enclosing "setup" (line 10),
        // and file-level pass should use first symbol "init" (line 1).
        let files = vec![
            (
                "src/main.js".to_string(),
                vec![make_symbol("init", 1), make_symbol("setup", 10)],
                vec![make_ref("./helper", ReferenceKind::Import, 15)],
            ),
            (
                "src/helper.js".to_string(),
                vec![make_symbol("helperFn", 1)],
                vec![],
            ),
        ];

        let edges = resolve_references(&files, Language::JavaScript, "repo:test:abc");
        let import_edges: Vec<_> = edges
            .iter()
            .filter(|e| e.edge_type == EdgeType::Imports)
            .collect();
        assert!(
            import_edges.is_empty(),
            "a module specifier alone cannot identify an exported symbol: {import_edges:?}"
        );
    }

    #[test]
    fn file_level_imports_edges_deduplicate() {
        // Two imports from the same source file to the same target file
        // should produce only one file-level IMPORTS edge (after dedup).
        let files = vec![
            (
                "src/main.js".to_string(),
                vec![make_symbol("main", 1)],
                vec![
                    make_ref("./helper", ReferenceKind::Import, 1),
                    make_ref("./helper", ReferenceKind::Import, 2),
                ],
            ),
            (
                "src/helper.js".to_string(),
                vec![make_symbol("helperFn", 1)],
                vec![],
            ),
        ];

        let edges = resolve_references(&files, Language::JavaScript, "repo:test:abc");
        let import_edges: Vec<_> = edges
            .iter()
            .filter(|e| e.edge_type == EdgeType::Imports)
            .collect();
        assert!(
            import_edges.is_empty(),
            "module imports without used bindings must not acquire owners: {import_edges:?}"
        );
    }

    #[test]
    fn file_level_imports_edges_for_multiple_targets() {
        // Imports to two different target files should produce edges to both
        let files = vec![
            (
                "src/main.js".to_string(),
                vec![make_symbol("main", 1)],
                vec![
                    make_ref("./helper", ReferenceKind::Import, 1),
                    make_ref("./utils", ReferenceKind::Import, 2),
                ],
            ),
            (
                "src/helper.js".to_string(),
                vec![make_symbol("helperFn", 1)],
                vec![],
            ),
            (
                "src/utils.js".to_string(),
                vec![make_symbol("utilFn", 1)],
                vec![],
            ),
        ];

        let edges = resolve_references(&files, Language::JavaScript, "repo:test:abc");
        let import_edges: Vec<_> = edges
            .iter()
            .filter(|e| e.edge_type == EdgeType::Imports)
            .collect();
        assert!(
            import_edges.is_empty(),
            "module imports without used bindings must not acquire owners: {import_edges:?}"
        );
    }

    #[test]
    fn resolved_edge_contains_evidence() {
        let files = vec![(
            "src/main.js".to_string(),
            vec![make_symbol("main", 1), make_symbol("greet", 10)],
            vec![make_ref("greet", ReferenceKind::Call, 5)],
        )];

        let edges = resolve_references(&files, Language::JavaScript, "repo:test:abc");
        let call = edges
            .iter()
            .find(|e| e.edge_type == EdgeType::Calls && !e.target_uid.starts_with("unresolved:"))
            .expect("should have a resolved CALLS edge");

        assert!(
            !call.evidence.is_empty(),
            "resolved edge should have evidence entries"
        );
        assert_eq!(call.evidence[0].kind, "same_file");
        assert!(call.evidence[0].weight > 0.0);
    }

    #[test]
    fn a_module_scope_reference_is_distinguishable_from_a_degenerate_span_guess() {
        // nw-349 (1). find_enclosing_symbol used to return Option<&RawSymbol>,
        // so "module scope" and "I guessed from a one-line symbol" arrived at
        // the call site as the SAME value. resolve_single_reference's `?`
        // discarded the former and silently trusted the latter.
        let mut one_liner = make_symbol("helper", 1);
        one_liner.end_line = 1; // a GENUINE one-line function
        let real = make_symbol("caller", 10); // spans 10..=15

        let syms: Vec<&RawSymbol> = {
            let mut v = vec![&one_liner, &real];
            v.sort_by_key(|s| s.start_line);
            v
        };

        // Inside a real span: exact.
        assert!(
            matches!(find_enclosing_symbol(&syms, 12), Enclosing::Exact(s) if s.name == "caller")
        );
        // After every span, with a degenerate code-bearing symbol available:
        // a GUESS, and the type must say so rather than looking like Exact.
        assert!(matches!(
            find_enclosing_symbol(&syms, 50),
            Enclosing::Degenerate(s) if s.name == "helper"
        ));
        // Before any symbol: module scope, which is a FACT, not a failure.
        assert!(matches!(
            find_enclosing_symbol(&syms, 0),
            Enclosing::ModuleScope
        ));
    }

    #[test]
    fn a_degenerate_span_fallback_never_attributes_a_call_to_a_data_symbol() {
        // nw-349 (1), measured on the checked-in fixtures BEFORE this guard:
        // testdata/cpp/simple.cpp:35 `logValue(temp);` was recorded as the
        // LOCAL VARIABLE `temp` calling `logValue`, and
        // testdata/python/simple.py:37 `main()` as `Property name` calling
        // `main`. A Constant/Variable/Property/Field has no body and cannot be
        // a call site, so an edge sourced at one is not a weak edge — it is a
        // fabricated one, indistinguishable from a real edge downstream.
        let mut var = make_symbol("temp", 34);
        var.end_line = 34;
        var.kind = SymbolKind::Variable;
        let mut func = make_symbol("setup", 31);
        func.end_line = 31; // the old C++ zero-height span
        let syms: Vec<&RawSymbol> = vec![&func, &var];

        match find_enclosing_symbol(&syms, 35) {
            Enclosing::Degenerate(s) => assert_eq!(
                s.name, "setup",
                "the fallback must skip data symbols; got {} ({:?})",
                s.name, s.kind
            ),
            other => panic!("expected a degenerate attribution, got {other:?}"),
        }
    }

    #[test]
    fn a_reference_after_a_lone_data_symbol_is_module_scope_not_a_fabricated_edge() {
        // The other half: with NO code-bearing degenerate symbol to fall back
        // on, the answer is `ModuleScope` — "this belongs to no symbol" — and
        // not "the constant did it". testdata/js/simple.js:28 is the measured
        // case: a module-top-level `greet()` recorded as `Constant dog`.
        let mut konst = make_symbol("dog", 20);
        konst.end_line = 20;
        konst.kind = SymbolKind::Constant;
        let syms: Vec<&RawSymbol> = vec![&konst];

        assert!(
            matches!(find_enclosing_symbol(&syms, 28), Enclosing::ModuleScope),
            "a Constant cannot call anything"
        );
    }

    #[test]
    fn an_assignment_target_on_the_call_line_is_module_scope_not_exact() {
        // nw-435. `lbug_version = _get_lbug_version()` in
        // scripts/pip-package/setup.py mints a one-line `Variable` for the
        // assignment target whose degenerate span (start_line == end_line)
        // sits on the SAME line as the call. That satisfies the EXACT-match
        // condition (`ref_line <= s.end_line.max(s.start_line)`), which —
        // unlike the degenerate fallback — had no kind check, so the call
        // resolved into the Variable instead of module scope.
        let mut target = make_symbol("lbug_version", 20);
        target.end_line = 20; // real one-line span, not the degenerate fallback
        target.kind = SymbolKind::Variable;
        let syms: Vec<&RawSymbol> = vec![&target];

        assert!(
            matches!(find_enclosing_symbol(&syms, 20), Enclosing::ModuleScope),
            "a call on the same line as an assignment target must not resolve \
             into the target — a Variable has no body and cannot be a call site"
        );
    }

    #[test]
    fn an_assignment_target_does_not_shadow_a_real_enclosing_function() {
        // Counterpart to the above: when the call line genuinely sits inside a
        // function body, a same-line data symbol earlier in the sort order
        // must not win the exact-match search just because it is nearer.
        // e.g. `def helper():\n    x = 1; return foo()` — `x` is a
        // degenerate-span Variable at the same line as a real call inside
        // `helper`'s span.
        let mut func = make_symbol("helper", 10);
        func.end_line = 15;
        let mut inline_var = make_symbol("x", 12);
        inline_var.end_line = 12;
        inline_var.kind = SymbolKind::Variable;
        let syms: Vec<&RawSymbol> = {
            let mut v = vec![&func, &inline_var];
            v.sort_by_key(|s| s.start_line);
            v
        };

        match find_enclosing_symbol(&syms, 12) {
            Enclosing::Exact(s) => assert_eq!(
                s.name, "helper",
                "must skip the data symbol and attribute to the enclosing function"
            ),
            other => panic!("expected Exact(helper), got {other:?}"),
        }
    }

    #[test]
    fn a_call_on_an_assignment_target_line_produces_no_edge_rather_than_a_wrong_one() {
        // End to end through resolve_references, mirroring
        // `a_call_after_a_constant_produces_no_edge_rather_than_a_wrong_one`
        // but for the EXACT-match branch: `lbug_version = _get_lbug_version()`
        // at module scope must not source an edge from `lbug_version`.
        let repo_uid = "repo:test:abc";
        let file_path = "setup.py";
        let mut target = make_symbol("lbug_version", 20);
        target.end_line = 20;
        target.kind = SymbolKind::Variable;
        let callee = make_symbol("_get_lbug_version", 1); // spans 1..=6
        let fabricated_source_uid =
            symbol_uid(repo_uid, file_path, &target.name, target.start_line);
        let files = vec![(
            file_path.to_string(),
            vec![callee, target],
            vec![make_ref(
                "_get_lbug_version",
                ReferenceKind::Call,
                20, // same line as the assignment target
            )],
        )];
        let edges = resolve_references(&files, Language::Python, repo_uid);
        assert!(
            !edges.iter().any(|e| e.source_uid == fabricated_source_uid),
            "the Variable assignment target must never be recorded as the \
             call's source: {edges:?}"
        );
    }

    #[test]
    fn a_call_after_a_constant_produces_no_edge_rather_than_a_wrong_one() {
        // End to end through resolve_references: the fabricated edge must not
        // reach the graph at all.
        let mut konst = make_symbol("dog", 20);
        konst.end_line = 20;
        konst.kind = SymbolKind::Constant;
        let greet = make_symbol("greet", 1); // spans 1..=6
        let files = vec![(
            "src/simple.js".to_string(),
            vec![greet, konst],
            vec![make_ref("greet", ReferenceKind::Call, 28)],
        )];
        let edges = resolve_references(&files, Language::JavaScript, "repo:test:abc");
        assert!(
            !edges.iter().any(|e| e.source_uid.contains("dog")),
            "a Constant must never be recorded as the source of a call: {edges:?}"
        );
    }

    #[test]
    fn find_enclosing_symbol_binary_search_correctness() {
        // Helper that runs both an end-line-aware linear scan and the binary
        // search, asserting they agree, then returns the start_line of the result.
        fn linear_scan(symbols: &[RawSymbol], ref_line: u32) -> Option<u32> {
            symbols
                .iter()
                .filter(|s| s.start_line <= ref_line && ref_line <= s.end_line.max(s.start_line))
                .max_by_key(|s| s.start_line)
                .map(|s| s.start_line)
        }

        fn check(symbols: &[RawSymbol], ref_line: u32) -> Option<u32> {
            // find_enclosing_symbol requires &[&RawSymbol]; build a sorted refs slice.
            let sorted: Vec<&RawSymbol> = {
                let mut v: Vec<&RawSymbol> = symbols.iter().collect();
                v.sort_by_key(|s| s.start_line);
                v
            };
            let binary = find_enclosing_symbol(&sorted, ref_line)
                .symbol()
                .map(|s| s.start_line);
            let linear = linear_scan(symbols, ref_line);
            assert_eq!(
                binary, linear,
                "binary search and linear scan disagree for ref_line={ref_line}"
            );
            binary
        }

        // Empty slice → None
        assert!(check(&[], 5).is_none());

        // make_symbol spans line..=line+5
        let symbols = vec![
            make_symbol("a", 10),
            make_symbol("b", 20),
            make_symbol("c", 30),
        ];

        // ref_line before all symbols → None
        assert!(check(&symbols, 5).is_none());

        // ref_line exactly on first symbol → first symbol
        assert_eq!(check(&symbols, 10), Some(10));

        // ref_line between first and second, inside first's span → first symbol
        assert_eq!(check(&symbols, 15), Some(10));

        // ref_line exactly on second symbol → second symbol
        assert_eq!(check(&symbols, 20), Some(20));

        // ref_line between second and third, inside second's span → second symbol
        assert_eq!(check(&symbols, 25), Some(20));

        // ref_line exactly on last symbol → last symbol
        assert_eq!(check(&symbols, 30), Some(30));

        // ref_line inside last symbol's span → last symbol
        assert_eq!(check(&symbols, 35), Some(30));

        // ref_line past every symbol's end → None (module-level code,
        // e.g. Python `if __name__ == '__main__':` after the last def)
        assert!(check(&symbols, 999).is_none());

        // ref_line in the gap between two symbols' spans → None
        let gapped = vec![make_symbol("a", 10), make_symbol("b", 30)];
        assert!(check(&gapped, 20).is_none());

        // Nested spans: ref past the inner symbol's end but inside the outer
        // one falls back to the outer symbol.
        let mut outer = make_symbol("outer", 10);
        outer.end_line = 100;
        let mut inner = make_symbol("inner", 20);
        inner.end_line = 30;
        let nested = vec![outer, inner];
        assert_eq!(check(&nested, 25), Some(20), "inside inner → inner");
        assert_eq!(
            check(&nested, 50),
            Some(10),
            "past inner's end but inside outer → outer"
        );
        assert!(check(&nested, 200).is_none(), "past outer's end → None");

        // Single symbol — before it → None
        let single = vec![make_symbol("only", 5)];
        assert!(check(&single, 1).is_none());

        // Single symbol — exactly on it → that symbol
        assert_eq!(check(&single, 5), Some(5));

        // Single symbol — inside its span → that symbol
        assert_eq!(check(&single, 8), Some(5));

        // Single symbol — past its end → None
        assert!(check(&single, 50).is_none());

        // Two symbols with same start_line — either is correct; just verify no panic
        // and that it agrees with linear scan.
        let dupes = vec![make_symbol("x", 10), make_symbol("y", 10)];
        let result = check(&dupes, 10);
        assert!(result.is_some());
    }

    #[test]
    fn find_enclosing_symbol_degenerate_span_fallback() {
        fn degenerate(name: &str, line: u32) -> RawSymbol {
            // Regex-based parsers (astro, svelte, vue, cobol) emit one-line spans.
            let mut sym = make_symbol(name, line);
            sym.end_line = line;
            sym
        }

        fn check(symbols: &[RawSymbol], ref_line: u32) -> Option<u32> {
            let sorted: Vec<&RawSymbol> = {
                let mut v: Vec<&RawSymbol> = symbols.iter().collect();
                v.sort_by_key(|s| s.start_line);
                v
            };
            find_enclosing_symbol(&sorted, ref_line)
                .symbol()
                .map(|s| s.start_line)
        }

        // A call ref on a later line is owned by the nearest preceding
        // degenerate-span symbol.
        let symbols = vec![degenerate("a", 10), degenerate("b", 20)];
        assert_eq!(check(&symbols, 25), Some(20));
        assert_eq!(check(&symbols, 999), Some(20));
        assert_eq!(check(&symbols, 12), Some(10));

        // Refs before the first symbol still get nothing.
        assert!(check(&symbols, 5).is_none());

        // Real-spanned symbols past their end must NOT claim the ref — the
        // fallback applies to degenerate spans only.
        let real = vec![make_symbol("real", 10)]; // spans 10..=15
        assert!(check(&real, 50).is_none());

        // Mixed file: a degenerate span still claims a later ref even when a
        // real-spanned symbol sits between them and has already ended.
        let mut short = make_symbol("short", 15);
        short.end_line = 16;
        let mixed = vec![degenerate("a", 10), short];
        assert_eq!(check(&mixed, 50), Some(10));
    }

    #[test]
    fn regex_parser_degenerate_spans_still_produce_call_edges() {
        // Svelte-like file: regex parser emits degenerate spans
        // (end_line == start_line) and Call references on later lines. The
        // intra-function call edge must survive the enclosing-span check.
        let mut helper = make_symbol("helper", 1);
        helper.end_line = 1;
        let mut caller = make_symbol("caller", 10);
        caller.end_line = 10;
        let files = vec![(
            "src/App.svelte".to_string(),
            vec![helper, caller],
            vec![make_ref("helper", ReferenceKind::Call, 12)],
        )];

        let edges = resolve_references(&files, Language::Svelte, "repo:test:abc");
        let call = edges
            .iter()
            .find(|e| e.edge_type == EdgeType::Calls && !e.target_uid.starts_with("unresolved:"))
            .expect("degenerate-span caller should still produce a resolved CALLS edge");
        assert!(
            call.source_uid.ends_with(":10"),
            "call edge should be attributed to caller (line 10), got {}",
            call.source_uid
        );
    }

    #[test]
    fn python_module_level_call_after_last_def_gets_no_edge() {
        // def main(): ...        lines 1-2
        // def dead_helper(): ... lines 4-5
        // if __name__ == '__main__': main()   lines 7-8 — module level
        let mut main_fn = make_symbol("main", 1);
        main_fn.end_line = 2;
        let mut dead_helper = make_symbol("dead_helper", 4);
        dead_helper.end_line = 5;

        let files = vec![(
            "app/__main__.py".to_string(),
            vec![main_fn, dead_helper],
            vec![make_ref("main", ReferenceKind::Call, 8)],
        )];

        let edges = resolve_references(&files, Language::Python, "repo:test:abc");
        assert!(
            edges.is_empty(),
            "module-level call after the last def must not be attributed to the \
             preceding function (phantom dead_helper → main edge); got: {edges:?}"
        );
    }

    #[test]
    fn python_call_inside_preceding_function_still_resolves() {
        // Same shape as the module-level case, but the call is inside
        // dead_helper's span — the enclosing-symbol attribution must survive.
        let mut main_fn = make_symbol("main", 1);
        main_fn.end_line = 2;
        let mut dead_helper = make_symbol("dead_helper", 4);
        dead_helper.end_line = 8;

        let files = vec![(
            "app/mod.py".to_string(),
            vec![main_fn, dead_helper],
            vec![make_ref("main", ReferenceKind::Call, 6)],
        )];

        let edges = resolve_references(&files, Language::Python, "repo:test:abc");
        let call = edges
            .iter()
            .find(|e| e.edge_type == EdgeType::Calls)
            .expect("call inside the function body should still produce an edge");
        let expected_source = symbol_uid("repo:test:abc", "app/mod.py", "dead_helper", 4);
        assert_eq!(call.source_uid, expected_source);
    }

    #[test]
    fn rust_cross_crate_qualified_call_resolves() {
        // crates/nestweaver-daemon/src/server.rs:
        //   use nestweaver_engine::rts_eval;
        //   fn run() { rts_eval::sidecar_path("x"); }
        // The qualified call must produce a CALLS edge to the engine crate's
        // sidecar_path, not "unresolved:sidecar_path".
        let mut sidecar_path = make_symbol("sidecar_path", 10);
        sidecar_path.end_line = 20;
        let mut run_fn = make_symbol("run", 3);
        run_fn.end_line = 10;

        let files = vec![
            (
                "crates/nestweaver-daemon/src/server.rs".to_string(),
                vec![run_fn],
                vec![
                    make_ref("nestweaver_engine::rts_eval", ReferenceKind::Import, 1),
                    make_ref("sidecar_path", ReferenceKind::Call, 5),
                ],
            ),
            (
                "crates/nestweaver-daemon/src/main.rs".to_string(),
                vec![make_symbol("main", 1)],
                vec![],
            ),
            (
                "crates/nestweaver-engine/src/lib.rs".to_string(),
                vec![make_symbol("Engine", 1)],
                vec![],
            ),
            (
                "crates/nestweaver-engine/src/rts_eval.rs".to_string(),
                vec![sidecar_path],
                vec![],
            ),
        ];

        let edges = resolve_references(&files, Language::Rust, "repo:test:abc");
        let call = edges
            .iter()
            .find(|e| e.edge_type == EdgeType::Calls)
            .expect("cross-crate call should produce a CALLS edge; got: {edges:?}");
        let expected_target = symbol_uid(
            "repo:test:abc",
            "crates/nestweaver-engine/src/rts_eval.rs",
            "sidecar_path",
            10,
        );
        assert_eq!(
            call.target_uid, expected_target,
            "qualified call must resolve to the sibling crate's function"
        );
        let expected_confidence = confidence_score(MatchType::ImportResolved, Language::Rust);
        assert!(
            (call.confidence - expected_confidence).abs() < f32::EPSILON,
            "expected import-resolved confidence {expected_confidence}, got {}",
            call.confidence
        );
    }

    #[test]
    fn rust_integration_test_use_crate_under_test_resolves() {
        // tests/beta_it.rs: `use fixture_repo::beta::b;` then `b()` — the
        // crate-under-test import must resolve so affected-tests sees the
        // test as a dependent of src/beta.rs.
        let mut b_fn = make_symbol("b", 3);
        b_fn.end_line = 8;
        let mut test_fn = make_symbol("it_works", 4);
        test_fn.end_line = 10;

        let files = vec![
            (
                "src/lib.rs".to_string(),
                vec![make_symbol("fixture_repo", 1)],
                vec![],
            ),
            ("src/beta.rs".to_string(), vec![b_fn], vec![]),
            (
                "tests/beta_it.rs".to_string(),
                vec![test_fn],
                vec![
                    make_ref("fixture_repo::beta::b", ReferenceKind::Import, 1),
                    make_ref("b", ReferenceKind::Call, 6),
                ],
            ),
        ];

        let edges = resolve_references(&files, Language::Rust, "repo:test:abc");

        // The import itself must resolve (IMPORTS edge into src/beta.rs)…
        let expected_target = symbol_uid("repo:test:abc", "src/beta.rs", "b", 3);
        let import_edge = edges
            .iter()
            .find(|e| e.edge_type == EdgeType::Imports && e.target_uid == expected_target);
        assert!(
            import_edge.is_some(),
            "integration-test import should create an IMPORTS edge into src/beta.rs; got: {edges:?}"
        );

        // …and the call must resolve to src/beta.rs's `b`, giving RTS a
        // CALLS dependency from the test to the changed file.
        let call = edges
            .iter()
            .find(|e| e.edge_type == EdgeType::Calls && e.target_uid == expected_target);
        assert!(
            call.is_some(),
            "call in integration test should resolve to src/beta.rs::b; got: {edges:?}"
        );
    }

    #[test]
    fn rust_external_crate_use_stays_unresolved() {
        // `use serde::Serialize;` must not be glued onto the local crate.
        let mut run_fn = make_symbol("run", 3);
        run_fn.end_line = 10;

        let files = vec![
            (
                "src/lib.rs".to_string(),
                vec![make_symbol("root", 1)],
                vec![],
            ),
            (
                "src/main.rs".to_string(),
                vec![run_fn],
                vec![make_ref("serde::Serialize", ReferenceKind::Import, 1)],
            ),
        ];

        let edges = resolve_references(&files, Language::Rust, "repo:test:abc");
        assert!(
            edges.iter().all(|e| e.edge_type != EdgeType::Imports),
            "external crate import must not create IMPORTS edges; got: {edges:?}"
        );
    }

    #[test]
    fn no_edge_to_symbol_never_referenced_in_source_file() {
        // Regression for the "phantom detect_entry_point → list_all_symbols
        // CALLS edge" bug: the resolver must never emit an edge whose
        // target name does not appear as a reference in the source file, even
        // with type environments active. (The DB-level instance of that bug
        // was a Symbol node carrying another symbol's UID — an engine/store
        // issue — but this guards the resolver side.)
        use crate::type_extractors::{BindingSource, TypeBinding};
        use crate::types::TypeEnvironment;

        let mut detect_entry_point = make_symbol("detect_entry_point", 1);
        detect_entry_point.end_line = 10;
        let mut detect_c = make_symbol("detect_c", 12);
        detect_c.end_line = 18;

        let store_struct = RawSymbol {
            name: "GraphStore".to_string(),
            kind: SymbolKind::Class,
            start_line: 1,
            end_line: 100,
            signature: "pub struct GraphStore".to_string(),
            content_hash: String::new(),
            is_entry_point: false,
            entry_point_kind: None,
            visibility: Visibility::Public,
            type_info: None,
            parent_name: None,
            scope_chain: None,
        };
        let mut list_all_symbols = make_symbol("list_all_symbols", 20);
        list_all_symbols.parent_name = Some("GraphStore".to_string());

        let files = vec![
            (
                "crates/nestweaver-parser/src/entry_points.rs".to_string(),
                vec![detect_entry_point, detect_c],
                // entry_points.rs calls only its own helpers; it never
                // mentions list_all_symbols nor imports the store.
                vec![make_ref("detect_c", ReferenceKind::Call, 5)],
            ),
            (
                "crates/nestweaver-store/src/read.rs".to_string(),
                vec![store_struct, list_all_symbols],
                vec![],
            ),
        ];

        // Type env with a high-confidence binding — the type-aware path must
        // still not fabricate a cross-file edge for a name absent from the file.
        let mut type_envs = std::collections::HashMap::new();
        let env = TypeEnvironment::from_bindings(vec![(
            "self".to_string(),
            1,
            TypeBinding {
                type_name: "GraphStore".to_string(),
                line: 1,
                confidence: 0.95,
                source: BindingSource::SelfThis,
            },
        )]);
        type_envs.insert(
            "crates/nestweaver-parser/src/entry_points.rs".to_string(),
            env,
        );

        let edges = resolve_references_with_context(
            &files,
            Language::Rust,
            "repo:test:abc",
            &WorkspaceContext::default(),
            Some(&type_envs),
            None,
        );

        let phantom_target = symbol_uid(
            "repo:test:abc",
            "crates/nestweaver-store/src/read.rs",
            "list_all_symbols",
            20,
        );
        assert!(
            edges.iter().all(|e| e.target_uid != phantom_target),
            "no edge may target a symbol the source file never references; got: {edges:?}"
        );
        // Sanity: the genuine same-file call still resolves.
        let real_target = symbol_uid(
            "repo:test:abc",
            "crates/nestweaver-parser/src/entry_points.rs",
            "detect_c",
            12,
        );
        assert!(
            edges
                .iter()
                .any(|e| e.edge_type == EdgeType::Calls && e.target_uid == real_target),
            "the genuine call must still resolve; got: {edges:?}"
        );
    }

    mod snapshot_tests {
        use super::*;
        use insta::assert_yaml_snapshot;

        fn sorted_edges(
            files: Vec<(String, Vec<RawSymbol>, Vec<RawReference>)>,
            language: Language,
        ) -> Vec<ResolvedEdge> {
            let mut edges = resolve_references(&files, language, "repo:test:snapshot");
            edges.sort_by(|a, b| {
                a.source_uid
                    .cmp(&b.source_uid)
                    .then_with(|| a.target_uid.cmp(&b.target_uid))
            });
            edges
        }

        #[test]
        fn snapshot_same_file_resolution() {
            let files = vec![(
                "src/main.js".to_string(),
                vec![
                    make_symbol("helper", 1),
                    make_symbol("caller", 10),
                    make_symbol("utils", 20),
                ],
                vec![
                    make_ref("helper", ReferenceKind::Call, 12),
                    make_ref("utils", ReferenceKind::Call, 15),
                ],
            )];
            assert_yaml_snapshot!(sorted_edges(files, Language::JavaScript));
        }

        #[test]
        fn snapshot_cross_file_resolution() {
            let files = vec![
                (
                    "src/main.js".to_string(),
                    vec![make_symbol("main", 5)],
                    vec![
                        make_ref("./helper", ReferenceKind::Import, 1),
                        make_binding("helperFn", "helperFn", "./helper", 1),
                        make_ref("helperFn", ReferenceKind::Call, 10),
                        make_ref("missingFn", ReferenceKind::Call, 15),
                    ],
                ),
                (
                    "src/helper.js".to_string(),
                    vec![make_symbol("helperFn", 1)],
                    vec![make_export("helperFn", "helperFn", None, 1)],
                ),
            ];
            assert_yaml_snapshot!(sorted_edges(files, Language::JavaScript));
        }

        #[test]
        fn snapshot_inheritance_resolution() {
            let files = vec![(
                "src/models.js".to_string(),
                vec![make_symbol("BaseModel", 1), make_symbol("UserModel", 20)],
                vec![make_ref("BaseModel", ReferenceKind::Extends, 21)],
            )];
            assert_yaml_snapshot!(sorted_edges(files, Language::JavaScript));
        }
    }

    #[test]
    fn type_aware_resolves_member_call_via_receiver_type() {
        use crate::type_extractors::{BindingSource, TypeBinding};
        use crate::types::TypeEnvironment;

        // File A: class Foo with method bar
        let mut foo_bar = make_symbol("bar", 5);
        foo_bar.parent_name = Some("Foo".to_string());
        foo_bar.kind = SymbolKind::Method;

        let foo_class = RawSymbol {
            name: "Foo".to_string(),
            kind: SymbolKind::Class,
            start_line: 1,
            end_line: 20,
            signature: "class Foo".to_string(),
            content_hash: String::new(),
            is_entry_point: false,
            entry_point_kind: None,
            visibility: Visibility::Public,
            type_info: None,
            parent_name: None,
            scope_chain: None,
        };

        // File B: class Baz with method bar (different parent)
        let mut baz_bar = make_symbol("bar", 5);
        baz_bar.kind = SymbolKind::Method;
        baz_bar.parent_name = Some("Baz".to_string());

        let baz_class = RawSymbol {
            name: "Baz".to_string(),
            kind: SymbolKind::Class,
            start_line: 1,
            end_line: 20,
            signature: "class Baz".to_string(),
            content_hash: String::new(),
            is_entry_point: false,
            entry_point_kind: None,
            visibility: Visibility::Public,
            type_info: None,
            parent_name: None,
            scope_chain: None,
        };

        // File C: caller that does foo_instance.bar()
        let caller = make_symbol("caller", 1);
        let bar_call = RawReference {
            scope: None,
            name: "bar".to_string(),
            kind: ReferenceKind::Call,
            start_line: 3,
            context: String::new(),
            receiver: Some("foo_instance".to_string()),
        };

        let files = vec![
            ("src/foo.ts".to_string(), vec![foo_class, foo_bar], vec![]),
            ("src/baz.ts".to_string(), vec![baz_class, baz_bar], vec![]),
            ("src/main.ts".to_string(), vec![caller], vec![bar_call]),
        ];

        // Build a type environment for main.ts: foo_instance has type Foo at line 2
        let mut type_envs = std::collections::HashMap::new();
        let env = TypeEnvironment::from_bindings(vec![(
            "foo_instance".to_string(),
            2,
            TypeBinding {
                type_name: "Foo".to_string(),
                line: 2,
                confidence: 0.9,
                source: BindingSource::Constructor,
            },
        )]);
        type_envs.insert("src/main.ts".to_string(), env);

        let edges = resolve_references_with_context(
            &files,
            Language::TypeScript,
            "repo:test:abc",
            &WorkspaceContext::default(),
            Some(&type_envs),
            None,
        );

        let call_edges: Vec<_> = edges
            .iter()
            .filter(|e| e.edge_type == EdgeType::Calls)
            .collect();
        assert_eq!(call_edges.len(), 1, "should have exactly one call edge");

        let edge = &call_edges[0];
        let expected_target = symbol_uid("repo:test:abc", "src/foo.ts", "bar", 5);
        let wrong_target = symbol_uid("repo:test:abc", "src/baz.ts", "bar", 5);
        assert_eq!(
            edge.target_uid, expected_target,
            "should resolve to Foo::bar in foo.ts"
        );
        assert_ne!(
            edge.target_uid, wrong_target,
            "should NOT resolve to Baz::bar"
        );
        assert!(
            (edge.confidence - 0.9).abs() < f32::EPSILON,
            "confidence should be min(0.9, 0.95) = 0.9, got {}",
            edge.confidence
        );
    }

    #[test]
    fn type_aware_self_receiver_resolves_to_own_class() {
        use crate::type_extractors::{BindingSource, TypeBinding};
        use crate::types::TypeEnvironment;

        // Class MyClass with method helper
        let mut helper = make_symbol("helper", 5);
        helper.parent_name = Some("MyClass".to_string());

        // Method doWork that calls this.helper()
        let mut do_work = make_symbol("doWork", 10);
        do_work.parent_name = Some("MyClass".to_string());

        let this_call = RawReference {
            scope: None,
            name: "helper".to_string(),
            kind: ReferenceKind::Call,
            start_line: 12,
            context: String::new(),
            receiver: Some("this".to_string()),
        };

        let files = vec![(
            "src/myclass.ts".to_string(),
            vec![
                RawSymbol {
                    name: "MyClass".to_string(),
                    kind: SymbolKind::Class,
                    start_line: 1,
                    end_line: 30,
                    signature: "class MyClass".to_string(),
                    content_hash: String::new(),
                    is_entry_point: false,
                    entry_point_kind: None,
                    visibility: Visibility::Public,
                    type_info: None,
                    parent_name: None,
                    scope_chain: None,
                },
                helper,
                do_work,
            ],
            vec![this_call],
        )];

        let mut type_envs = std::collections::HashMap::new();
        let env = TypeEnvironment::from_bindings(vec![(
            "this".to_string(),
            10,
            TypeBinding {
                type_name: "MyClass".to_string(),
                line: 10,
                confidence: 0.95,
                source: BindingSource::SelfThis,
            },
        )]);
        type_envs.insert("src/myclass.ts".to_string(), env);

        let edges = resolve_references_with_context(
            &files,
            Language::TypeScript,
            "repo:test:abc",
            &WorkspaceContext::default(),
            Some(&type_envs),
            None,
        );

        let call_edges: Vec<_> = edges
            .iter()
            .filter(|e| e.edge_type == EdgeType::Calls)
            .collect();
        assert_eq!(call_edges.len(), 1, "should have exactly one call edge");

        let edge = &call_edges[0];
        let expected_target = symbol_uid("repo:test:abc", "src/myclass.ts", "helper", 5);
        assert_eq!(
            edge.target_uid, expected_target,
            "should resolve to helper method"
        );
        assert!(
            (edge.confidence - 0.95).abs() < f32::EPSILON,
            "confidence should be capped at 0.95, got {}",
            edge.confidence
        );
    }

    #[test]
    fn mro_walk_finds_inherited_method() {
        use crate::type_extractors::{BindingSource, TypeBinding};
        use crate::types::TypeEnvironment;

        // BaseClass has method "save" at line 5
        let mut save_method = make_symbol("save", 5);
        save_method.kind = SymbolKind::Method;
        save_method.parent_name = Some("BaseClass".to_string());

        let base_class = RawSymbol {
            name: "BaseClass".to_string(),
            kind: SymbolKind::Class,
            start_line: 1,
            end_line: 20,
            signature: "class BaseClass".to_string(),
            content_hash: String::new(),
            is_entry_point: false,
            entry_point_kind: None,
            visibility: Visibility::Public,
            type_info: None,
            parent_name: None,
            scope_chain: None,
        };

        // ChildClass extends BaseClass (no "save" method of its own)
        let child_class = RawSymbol {
            name: "ChildClass".to_string(),
            kind: SymbolKind::Class,
            start_line: 30,
            end_line: 50,
            signature: "class ChildClass extends BaseClass".to_string(),
            content_hash: String::new(),
            is_entry_point: false,
            entry_point_kind: None,
            visibility: Visibility::Public,
            type_info: None,
            parent_name: None,
            scope_chain: None,
        };

        let extends_ref = RawReference {
            scope: None,
            name: "BaseClass".to_string(),
            kind: ReferenceKind::Extends,
            start_line: 30,
            context: String::new(),
            receiver: None,
        };

        // Caller file: child_instance.save()
        let caller = make_symbol("caller", 1);
        let save_call = RawReference {
            scope: None,
            name: "save".to_string(),
            kind: ReferenceKind::Call,
            start_line: 5,
            context: String::new(),
            receiver: Some("child_instance".to_string()),
        };

        let files = vec![
            (
                "src/base.ts".to_string(),
                vec![base_class, save_method],
                vec![],
            ),
            (
                "src/child.ts".to_string(),
                vec![child_class],
                vec![extends_ref],
            ),
            ("src/main.ts".to_string(), vec![caller], vec![save_call]),
        ];

        // Type environment: child_instance has type ChildClass
        let mut type_envs = std::collections::HashMap::new();
        let env = TypeEnvironment::from_bindings(vec![(
            "child_instance".to_string(),
            2,
            TypeBinding {
                type_name: "ChildClass".to_string(),
                line: 2,
                confidence: 0.9,
                source: BindingSource::Constructor,
            },
        )]);
        type_envs.insert("src/main.ts".to_string(), env);

        let edges = resolve_references_with_context(
            &files,
            Language::TypeScript,
            "repo:test:abc",
            &WorkspaceContext::default(),
            Some(&type_envs),
            None,
        );

        let call_edges: Vec<_> = edges
            .iter()
            .filter(|e| e.edge_type == EdgeType::Calls)
            .collect();
        assert_eq!(
            call_edges.len(),
            1,
            "should have exactly one call edge; got: {call_edges:?}"
        );

        let edge = &call_edges[0];
        let expected_target = symbol_uid("repo:test:abc", "src/base.ts", "save", 5);
        assert_eq!(
            edge.target_uid, expected_target,
            "should resolve to BaseClass::save via MRO walk"
        );

        // Confidence should decay multiplicatively: 0.9 * 0.95 = 0.855
        assert!(
            (edge.confidence - 0.855).abs() < 0.01,
            "confidence should be ~0.855 (0.9 * 0.95 for one hop), got {}",
            edge.confidence
        );
    }

    #[test]
    fn type_aware_falls_back_to_name_based_when_no_type_env() {
        // Same setup as type_aware test but without type_envs
        // Should fall through to name-based resolution
        let mut foo_bar = make_symbol("bar", 5);
        foo_bar.parent_name = Some("Foo".to_string());

        let caller = make_symbol("caller", 1);
        let bar_call = RawReference {
            scope: None,
            name: "bar".to_string(),
            kind: ReferenceKind::Call,
            start_line: 3,
            context: String::new(),
            receiver: Some("foo_instance".to_string()),
        };

        let files = vec![
            ("src/foo.ts".to_string(), vec![foo_bar], vec![]),
            ("src/main.ts".to_string(), vec![caller], vec![bar_call]),
        ];

        // No type_envs → passes None
        let edges = resolve_references_with_context(
            &files,
            Language::TypeScript,
            "repo:test:abc",
            &WorkspaceContext::default(),
            None,
            None,
        );

        let call_edges: Vec<_> = edges
            .iter()
            .filter(|e| e.edge_type == EdgeType::Calls)
            .collect();
        assert!(
            !call_edges.is_empty(),
            "should still produce edges via name-based fallback"
        );
    }

    #[test]
    fn parallel_resolution_produces_identical_edges() {
        // Build a multi-file fixture with cross-file calls, imports, extends,
        // same-file references, and type references. Run resolution twice and
        // assert the sorted edge vectors are identical (determinism check).
        fn make_class(name: &str, line: u32) -> RawSymbol {
            RawSymbol {
                name: name.to_string(),
                kind: SymbolKind::Class,
                start_line: line,
                end_line: line + 20,
                signature: format!("class {name}"),
                content_hash: String::new(),
                is_entry_point: false,
                entry_point_kind: None,
                visibility: Visibility::Public,
                type_info: None,
                parent_name: None,
                scope_chain: None,
            }
        }

        fn make_method(name: &str, line: u32, parent: &str) -> RawSymbol {
            RawSymbol {
                name: name.to_string(),
                kind: SymbolKind::Function,
                start_line: line,
                end_line: line,
                signature: format!("function {name}()"),
                content_hash: String::new(),
                is_entry_point: false,
                entry_point_kind: None,
                visibility: Visibility::Inferred,
                type_info: None,
                parent_name: Some(parent.to_string()),
                scope_chain: None,
            }
        }

        let files = vec![
            // File 1: base module with two functions
            (
                "src/base.ts".to_string(),
                vec![
                    make_class("Base", 1),
                    make_method("save", 5, "Base"),
                    make_symbol("validate", 15),
                ],
                vec![],
            ),
            // File 2: child extending base
            (
                "src/child.ts".to_string(),
                vec![make_class("Child", 1), make_symbol("process", 10)],
                vec![
                    make_ref("Base", ReferenceKind::Extends, 1),
                    make_ref("./base", ReferenceKind::Import, 1),
                    make_ref("validate", ReferenceKind::Call, 12),
                ],
            ),
            // File 3: utils with helpers
            (
                "src/utils.ts".to_string(),
                vec![
                    make_symbol("format", 1),
                    make_symbol("parse", 10),
                    make_symbol("transform", 20),
                ],
                vec![make_ref("format", ReferenceKind::Call, 15)],
            ),
            // File 4: service importing child and utils
            (
                "src/service.ts".to_string(),
                vec![
                    make_symbol("init", 1),
                    make_symbol("run", 10),
                    make_symbol("cleanup", 20),
                ],
                vec![
                    make_ref("./child", ReferenceKind::Import, 1),
                    make_ref("./utils", ReferenceKind::Import, 2),
                    make_ref("process", ReferenceKind::Call, 12),
                    make_ref("format", ReferenceKind::Call, 14),
                    make_ref("transform", ReferenceKind::Call, 22),
                ],
            ),
            // File 5: tests importing service
            (
                "src/test.ts".to_string(),
                vec![make_symbol("testInit", 1), make_symbol("testRun", 10)],
                vec![
                    make_ref("./service", ReferenceKind::Import, 1),
                    make_ref("init", ReferenceKind::Call, 5),
                    make_ref("run", ReferenceKind::Call, 12),
                    make_ref("unknownFn", ReferenceKind::Call, 15),
                ],
            ),
            // File 6: another consumer in same directory
            (
                "src/consumer.ts".to_string(),
                vec![make_symbol("consume", 1)],
                vec![
                    make_ref("parse", ReferenceKind::Call, 3),
                    make_ref("Base", ReferenceKind::TypeRef, 5),
                ],
            ),
        ];

        let sort_key = |e: &ResolvedEdge| {
            (
                e.source_uid.clone(),
                e.target_uid.clone(),
                format!("{:?}", e.edge_type),
            )
        };

        let mut edges1 = resolve_references(&files, Language::TypeScript, "repo:test:determinism");
        edges1.sort_by_key(|e| sort_key(e));

        let mut edges2 = resolve_references(&files, Language::TypeScript, "repo:test:determinism");
        edges2.sort_by_key(|e| sort_key(e));

        assert_eq!(
            edges1.len(),
            edges2.len(),
            "edge counts must match across runs"
        );
        for (i, (e1, e2)) in edges1.iter().zip(edges2.iter()).enumerate() {
            assert_eq!(
                e1.source_uid, e2.source_uid,
                "source_uid mismatch at edge {i}"
            );
            assert_eq!(
                e1.target_uid, e2.target_uid,
                "target_uid mismatch at edge {i}"
            );
            assert_eq!(e1.edge_type, e2.edge_type, "edge_type mismatch at edge {i}");
            assert!(
                (e1.confidence - e2.confidence).abs() < f32::EPSILON,
                "confidence mismatch at edge {i}: {} vs {}",
                e1.confidence,
                e2.confidence,
            );
        }
    }

    #[test]
    fn type_aware_chained_dot_receiver_resolves_self_store_query() {
        use crate::type_extractors::{BindingSource, TypeBinding};
        use crate::types::TypeEnvironment;

        // Store class with method `query`
        let store_class = RawSymbol {
            name: "Store".to_string(),
            kind: SymbolKind::Class,
            start_line: 1,
            end_line: 20,
            signature: "class Store".to_string(),
            content_hash: String::new(),
            is_entry_point: false,
            entry_point_kind: None,
            visibility: Visibility::Public,
            type_info: None,
            parent_name: None,
            scope_chain: None,
        };
        let mut store_query = make_symbol("query", 5);
        store_query.parent_name = Some("Store".to_string());

        // MyService class with method `handle` that calls `self.store.query()`
        let service_class = RawSymbol {
            name: "MyService".to_string(),
            kind: SymbolKind::Class,
            start_line: 1,
            end_line: 30,
            signature: "class MyService".to_string(),
            content_hash: String::new(),
            is_entry_point: false,
            entry_point_kind: None,
            visibility: Visibility::Public,
            type_info: None,
            parent_name: None,
            scope_chain: None,
        };
        let mut handle_method = make_symbol("handle", 10);
        handle_method.parent_name = Some("MyService".to_string());

        let chained_call = RawReference {
            scope: None,
            name: "query".to_string(),
            kind: ReferenceKind::Call,
            start_line: 12,
            context: String::new(),
            receiver: Some("self.store".to_string()),
        };

        let files = vec![
            (
                "src/store.rs".to_string(),
                vec![store_class, store_query],
                vec![],
            ),
            (
                "src/service.rs".to_string(),
                vec![service_class, handle_method],
                vec![chained_call],
            ),
        ];

        // Type env for service.rs:
        //   self → MyService at line 5 (enclosing class)
        //   store → Store at line 2 (field binding)
        let mut type_envs = std::collections::HashMap::new();
        let env = TypeEnvironment::from_bindings(vec![
            (
                "self".to_string(),
                5,
                TypeBinding {
                    type_name: "MyService".to_string(),
                    line: 5,
                    confidence: 1.0,
                    source: BindingSource::SelfThis,
                },
            ),
            (
                "store".to_string(),
                2,
                TypeBinding {
                    type_name: "Store".to_string(),
                    line: 2,
                    confidence: 0.9,
                    source: BindingSource::Annotation,
                },
            ),
        ]);
        type_envs.insert("src/service.rs".to_string(), env);

        let edges = resolve_references_with_context(
            &files,
            Language::TypeScript,
            "repo:test:abc",
            &WorkspaceContext::default(),
            Some(&type_envs),
            None,
        );

        let call_edges: Vec<_> = edges
            .iter()
            .filter(|e| e.edge_type == EdgeType::Calls)
            .collect();
        assert_eq!(call_edges.len(), 1, "should have exactly one call edge");

        let edge = &call_edges[0];
        let expected_target = symbol_uid("repo:test:abc", "src/store.rs", "query", 5);
        assert_eq!(
            edge.target_uid, expected_target,
            "self.store.query() should resolve to Store::query in store.rs"
        );
    }

    fn make_alias_ref(alias: &str, specifier: &str, line: u32) -> RawReference {
        RawReference {
            scope: None,
            name: alias.to_string(),
            kind: ReferenceKind::ImportAlias,
            start_line: line,
            context: specifier.to_string(),
            receiver: None,
        }
    }

    #[test]
    fn rust_aliased_import_call_resolves_to_original() {
        // use crate::config::load as load_config;
        // fn f() { load_config(); }
        let files = vec![
            (
                "src/main.rs".to_string(),
                vec![make_symbol("f", 3)],
                vec![
                    make_ref("crate::config::load", ReferenceKind::Import, 1),
                    make_alias_ref("load_config", "crate::config::load", 1),
                    make_ref("load_config", ReferenceKind::Call, 4),
                ],
            ),
            (
                "src/config.rs".to_string(),
                vec![make_symbol("load", 1)],
                vec![],
            ),
        ];

        let edges = resolve_references(&files, Language::Rust, "repo:test:abc");
        let call_edges: Vec<_> = edges
            .iter()
            .filter(|e| e.edge_type == EdgeType::Calls)
            .collect();
        assert_eq!(
            call_edges.len(),
            1,
            "expected one call edge; got: {edges:?}"
        );

        let expected_target = symbol_uid("repo:test:abc", "src/config.rs", "load", 1);
        assert_eq!(
            call_edges[0].target_uid, expected_target,
            "call through alias should resolve to config::load"
        );
        let expected_confidence = confidence_score(MatchType::ImportResolved, Language::Rust);
        assert!(
            (call_edges[0].confidence - expected_confidence).abs() < f32::EPSILON,
            "alias-resolved call should have ImportResolved confidence"
        );
    }

    #[test]
    fn rust_mixed_use_list_alias_and_plain() {
        // use crate::util::{c as d, e};
        // fn f() { d(); e(); }
        let files = vec![
            (
                "src/main.rs".to_string(),
                vec![make_symbol("f", 3)],
                vec![
                    make_ref("crate::util::c", ReferenceKind::Import, 1),
                    make_alias_ref("d", "crate::util::c", 1),
                    make_ref("crate::util::e", ReferenceKind::Import, 1),
                    make_ref("d", ReferenceKind::Call, 4),
                    make_ref("e", ReferenceKind::Call, 5),
                ],
            ),
            (
                "src/util.rs".to_string(),
                vec![make_symbol("c", 1), make_symbol("e", 10)],
                vec![],
            ),
        ];

        let edges = resolve_references(&files, Language::Rust, "repo:test:abc");
        let call_targets: Vec<_> = edges
            .iter()
            .filter(|e| e.edge_type == EdgeType::Calls)
            .map(|e| e.target_uid.as_str())
            .collect();
        assert_eq!(
            call_targets,
            [
                symbol_uid("repo:test:abc", "src/util.rs", "c", 1),
                symbol_uid("repo:test:abc", "src/util.rs", "e", 10),
            ],
            "aliased and plain imports from a mixed use list should both resolve"
        );
    }

    #[test]
    fn rust_local_symbol_shadows_import_alias() {
        // Precedence: a same-file symbol wins over an import alias of the
        // same name.
        let files = vec![
            (
                "src/main.rs".to_string(),
                vec![make_symbol("f", 3), make_symbol("load_config", 10)],
                vec![
                    make_ref("crate::config::load", ReferenceKind::Import, 1),
                    make_alias_ref("load_config", "crate::config::load", 1),
                    make_ref("load_config", ReferenceKind::Call, 4),
                ],
            ),
            (
                "src/config.rs".to_string(),
                vec![make_symbol("load", 1)],
                vec![],
            ),
        ];

        let edges = resolve_references(&files, Language::Rust, "repo:test:abc");
        let call_edges: Vec<_> = edges
            .iter()
            .filter(|e| e.edge_type == EdgeType::Calls)
            .collect();
        assert_eq!(
            call_edges.len(),
            1,
            "expected one call edge; got: {edges:?}"
        );

        let expected_target = symbol_uid("repo:test:abc", "src/main.rs", "load_config", 10);
        assert_eq!(
            call_edges[0].target_uid, expected_target,
            "local symbol should shadow the import alias"
        );
        let expected_confidence = confidence_score(MatchType::SameFileExact, Language::Rust);
        assert!(
            (call_edges[0].confidence - expected_confidence).abs() < f32::EPSILON,
            "shadowed alias should resolve with SameFileExact confidence"
        );
    }
}

#[cfg(test)]
mod user_pain_reference_tests {
    use super::*;
    use nestweaver_parser::parse_source;
    use std::path::Path;

    #[test]
    fn actual_embedding_base_contains_has_only_explicit_callers() {
        let path = "crates/nestweaver-store/src/search.rs";
        let source = format!(
            "{}\nfn review_typed_embedding_base_contains(base: &EmbeddingBase, uid: &str) -> bool {{\n base.contains(uid)\n}}\n",
            include_str!("../../nestweaver-store/src/search.rs")
        );
        let parsed = parse_source(Path::new(path), &source).unwrap();
        let env = crate::types::TypeEnvironment::build(
            &source,
            Language::Rust,
            &parsed.symbols,
            &parsed.type_bindings,
        );
        let files = vec![(path.to_string(), parsed.symbols, parsed.references)];
        for witness in [
            "binary_v2_round_trip_binds_identity_pipeline_and_payload",
            "an_implausible_embedding_count_is_rejected_not_allocated",
            "binary_atomic_replace_cleans_partial_temp_after_write_error",
            "binary_save_reports_parent_sync_failure_and_reopens_complete_replacement",
            "binary_load_rejects_bad_magic",
        ] {
            let symbol = files[0]
                .1
                .iter()
                .find(|symbol| symbol.name == witness)
                .expect("actual error-string witness");
            let calls: Vec<_> = files[0]
                .2
                .iter()
                .filter(|reference| {
                    reference.kind == ReferenceKind::Call
                        && reference.name == "contains"
                        && reference.start_line >= symbol.start_line
                        && reference.start_line <= symbol.end_line
                })
                .collect();
            assert!(
                !calls.is_empty(),
                "actual witness {witness} must retain its calls"
            );
            assert!(
                calls
                    .iter()
                    .all(|reference| reference
                        .receiver
                        .as_deref()
                        .is_some_and(|receiver| receiver.contains("to_string()")
                            || receiver.contains("format!"))),
                "actual string-result receivers must retain their syntax: {witness}: {calls:#?}"
            );
        }
        let envs = std::collections::HashMap::from([(path.to_string(), env)]);
        let edges = resolve_references_with_context(
            &files,
            Language::Rust,
            "repo:test:abc",
            &WorkspaceContext::default(),
            Some(&envs),
            None,
        );
        let target = uid(&files, path, "contains");
        // Closure and field-chain typing remains unsupported; prove the known
        // parameter case while retaining all real string-result counterweights.
        let allowed = std::collections::BTreeSet::from([uid(
            &files,
            path,
            "review_typed_embedding_base_contains",
        )]);
        let incoming: Vec<_> = edges
            .iter()
            .filter(|edge| edge.edge_type == EdgeType::Calls && edge.target_uid == target)
            .collect();
        let actual: std::collections::BTreeSet<_> = incoming
            .iter()
            .map(|edge| edge.source_uid.clone())
            .collect();
        assert_eq!(
            actual, allowed,
            "EmbeddingBase::contains must retain exactly its supported typed caller: {incoming:#?}"
        );
    }

    #[test]
    fn actual_regex_reader_pool_len_has_only_its_explicit_caller() {
        let path = "crates/nestweaver-store/src/regex_index.rs";
        let source = format!(
            "{}\nfn review_typed_regex_reader_pool_len(pool: &RegexReaderPool) -> usize {{\n pool.len()\n}}\n",
            include_str!("../../nestweaver-store/src/regex_index.rs")
        );
        let parsed = parse_source(Path::new(path), &source).unwrap();
        let env = crate::types::TypeEnvironment::build(
            &source,
            Language::Rust,
            &parsed.symbols,
            &parsed.type_bindings,
        );
        let files = vec![(path.to_string(), parsed.symbols, parsed.references)];
        let saturated = files[0]
            .1
            .iter()
            .find(|symbol| {
                symbol.name == "saturated_candidate_query_widens_instead_of_dropping_matches"
            })
            .expect("actual saturated candidate witness");
        let macro_chain_line = source
            .lines()
            .enumerate()
            .find_map(|(index, line)| {
                let line_number = index as u32 + 1;
                (line_number >= saturated.start_line
                    && line_number <= saturated.end_line
                    && line.trim() == ".len(),")
                    .then_some(line_number)
            })
            .expect("actual multiline macro-result len witness");
        let macro_chain = files[0]
            .2
            .iter()
            .find(|reference| {
                reference.kind == ReferenceKind::Call
                    && reference.name == "len"
                    && reference.start_line == macro_chain_line
            })
            .expect("parser must retain the actual chained macro call");
        let source_lines = source.lines().collect::<Vec<_>>();
        let preceding = source_lines[..(macro_chain_line - 1) as usize]
            .iter()
            .rev()
            .take(2)
            .copied()
            .collect::<Vec<_>>();
        assert_eq!(
            preceding,
            vec!["                .unwrap()", "                .unwrap()"]
        );
        assert!(
            macro_chain.receiver.is_some(),
            "a method on a macro call result must retain receiver evidence: {macro_chain:#?}"
        );
        let envs = std::collections::HashMap::from([(path.to_string(), env)]);
        let edges = resolve_references_with_context(
            &files,
            Language::Rust,
            "repo:test:abc",
            &WorkspaceContext::default(),
            Some(&envs),
            None,
        );
        let target = uid(&files, path, "len");
        let allowed = uid(&files, path, "review_typed_regex_reader_pool_len");
        let incoming: Vec<_> = edges
            .iter()
            .filter(|edge| edge.edge_type == EdgeType::Calls && edge.target_uid == target)
            .collect();
        let actual: std::collections::BTreeSet<_> = incoming
            .iter()
            .map(|edge| edge.source_uid.clone())
            .collect();
        assert_eq!(
            actual,
            std::collections::BTreeSet::from([allowed]),
            "RegexReaderPool::len must retain its supported typed caller; macro metadata: {macro_chain:#?}; incoming: {incoming:#?}"
        );
    }

    #[test]
    fn parsed_multiline_lock_chain_does_not_call_its_enclosing_len() {
        let path = "src/regex_index.rs";
        let source = r#"struct RegexReaderPool { shards: Mutex<Vec<Reader>> }
static POOL: RegexReaderPool = todo!();
impl RegexReaderPool {
    fn len(&self) -> usize {
        self.shards
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .len()
    }
    fn direct(&self) -> usize {
        self.len()
    }
}
fn known() -> usize {
    POOL.len()
}
"#;
        let parsed = parse_source(Path::new(path), source).unwrap();
        let env = crate::types::TypeEnvironment::build(
            source,
            Language::Rust,
            &parsed.symbols,
            &parsed.type_bindings,
        );
        let files = vec![(path.to_string(), parsed.symbols, parsed.references)];
        let chain = files[0]
            .2
            .iter()
            .find(|reference| {
                reference.kind == ReferenceKind::Call
                    && reference.name == "len"
                    && reference
                        .receiver
                        .as_deref()
                        .is_some_and(|receiver| receiver.contains("lock()"))
            })
            .expect("parser must capture the actual multiline chain");
        assert!(
            chain.receiver.as_deref().is_some_and(|receiver| {
                receiver.contains("lock()") && receiver.contains("unwrap_or_else")
            }),
            "{chain:?}"
        );
        let known = files[0]
            .2
            .iter()
            .find(|reference| {
                reference.kind == ReferenceKind::Call
                    && reference.name == "len"
                    && reference.receiver.as_deref() == Some("POOL")
            })
            .expect("parser must capture the typed direct receiver");
        assert_eq!(
            env.lookup("POOL", known.start_line)
                .map(|binding| binding.type_name.as_str()),
            Some("RegexReaderPool"),
            "the positive witness needs an actual receiver type binding"
        );
        let envs = std::collections::HashMap::from([(path.to_string(), env)]);
        let edges = resolve_references_with_context(
            &files,
            Language::Rust,
            "repo:test:abc",
            &WorkspaceContext::default(),
            Some(&envs),
            None,
        );
        let len = uid(&files, path, "len");
        assert!(
            !edges.iter().any(|edge| {
                edge.edge_type == EdgeType::Calls
                    && edge.source_uid == len
                    && edge.target_uid == len
            }),
            "lock result must not inherit the enclosing self type: {edges:#?}"
        );
        for caller in ["direct", "known"] {
            assert!(
                edges.iter().any(|edge| {
                    edge.edge_type == EdgeType::Calls
                        && edge.source_uid == uid(&files, path, caller)
                        && edge.target_uid == len
                        && edge.evidence.iter().any(|e| e.kind == "type_aware")
                }),
                "known direct receiver {caller} must still resolve: {edges:#?}"
            );
        }
    }

    fn parsed_files(inputs: &[(&str, &str)]) -> Vec<(String, Vec<RawSymbol>, Vec<RawReference>)> {
        inputs
            .iter()
            .map(|(path, source)| {
                let parsed = parse_source(Path::new(path), source).unwrap();
                (path.to_string(), parsed.symbols, parsed.references)
            })
            .collect()
    }

    fn uid(
        files: &[(String, Vec<RawSymbol>, Vec<RawReference>)],
        path: &str,
        name: &str,
    ) -> String {
        let symbol = files
            .iter()
            .find(|(file, _, _)| file == path)
            .unwrap()
            .1
            .iter()
            .find(|symbol| symbol.name == name)
            .unwrap();
        symbol_uid("repo:test:abc", path, name, symbol.start_line)
    }

    #[test]
    fn used_import_bindings_connect_actual_users_without_file_proxies() {
        for import_and_call in [
            (
                "import { used as selected, unused } from './helper.js';",
                "selected()",
            ),
            ("import * as helper from './helper.js';", "helper.used()"),
            ("const helper = require('./helper.js');", "helper.used()"),
            (
                "const { used: selected, unused } = require('./helper.js');",
                "selected()",
            ),
        ] {
            let consumer = format!(
                "{}\nconst unrelatedFirst = 1;\nfunction user() {{\n  return {};\n}}\nfunction unrelated() {{ return 2; }}\n",
                import_and_call.0, import_and_call.1
            );
            let files = parsed_files(&[
                (
                    "src/helper.js",
                    "export function unused() { return 0; }\nexport function used() { return 1; }\nfunction privateHelper() { return 2; }\n",
                ),
                ("src/consumer.js", &consumer),
            ]);
            let edges = resolve_references(&files, Language::JavaScript, "repo:test:abc");
            let imports: Vec<_> = edges
                .iter()
                .filter(|edge| edge.edge_type == EdgeType::Imports)
                .map(|edge| (edge.source_uid.clone(), edge.target_uid.clone()))
                .collect();
            assert_eq!(
                imports,
                vec![(
                    uid(&files, "src/consumer.js", "user"),
                    uid(&files, "src/helper.js", "used")
                )],
                "{}: {edges:#?}",
                import_and_call.0
            );
            assert!(
                edges.iter().any(|edge| edge.edge_type == EdgeType::Calls
                    && edge.source_uid == uid(&files, "src/consumer.js", "user")
                    && edge.target_uid == uid(&files, "src/helper.js", "used")),
                "actual call missing: {edges:#?}"
            );
        }
    }

    #[test]
    fn unused_side_effect_and_shadowed_imports_do_not_attribute_symbols() {
        let files = parsed_files(&[
            (
                "src/helper.js",
                "export function selected() { return 1; }\nexport function unused() { return 0; }\n",
            ),
            (
                "src/consumer.js",
                "import { selected, unused } from './helper.js';\nimport './helper.js';\nconst first = 1;\nfunction shadow(selected) {\n  return selected();\n}\nfunction unrelated() { return 2; }\n",
            ),
        ]);
        let edges = resolve_references(&files, Language::JavaScript, "repo:test:abc");
        assert!(
            !edges.iter().any(|edge| edge.edge_type == EdgeType::Imports),
            "unused/shadowed imports acquired owners: {edges:#?}"
        );
        assert!(
            !edges.iter().any(|edge| edge.target_uid.starts_with("sym:")
                && edge.target_uid == uid(&files, "src/helper.js", "selected")),
            "shadowed parameter bound to import: {edges:#?}"
        );
    }

    #[test]
    fn public_name_alias_is_not_a_second_export_of_the_local_name() {
        for (exporter, valid_import, valid_target, invalid_import) in [
            (
                "function actual() {}\nmodule.exports = { renamed: actual };\n",
                "const { renamed } = require('./helper.js');",
                "actual",
                "const { actual } = require('./helper.js');",
            ),
            (
                "function Router() {}\nexport { Router as default };\n",
                "import renamed from './helper.js';",
                "Router",
                "import { Router as actual } from './helper.js';",
            ),
        ] {
            let valid_call = "renamed()";
            let valid = format!("{valid_import}\nfunction user() {{\n  {valid_call};\n}}\n");
            let files = parsed_files(&[("src/helper.js", exporter), ("src/user.js", &valid)]);
            let edges = resolve_references(&files, Language::JavaScript, "repo:test:abc");
            assert!(
                edges
                    .iter()
                    .any(|edge| edge.source_uid == uid(&files, "src/user.js", "user")
                        && edge.target_uid == uid(&files, "src/helper.js", valid_target)
                        && edge.edge_type == EdgeType::Calls),
                "valid public alias: {edges:#?}"
            );
            let invalid = format!("{invalid_import}\nfunction user() {{\n  actual();\n}}\n");
            let files = parsed_files(&[("src/helper.js", exporter), ("src/user.js", &invalid)]);
            let edges = resolve_references(&files, Language::JavaScript, "repo:test:abc");
            assert!(
                !edges
                    .iter()
                    .any(|edge| edge.target_uid == uid(&files, "src/helper.js", valid_target)),
                "local declaration was not exported under its own name: {edges:#?}"
            );
        }
        for name in ["actual", "renamed"] {
            let consumer = format!(
                "import {{ {name} }} from './helper.js';\nfunction user() {{\n  {name}();\n}}\n"
            );
            let files = parsed_files(&[
                (
                    "src/helper.js",
                    "export function actual() {}\nexport { actual as renamed };\n",
                ),
                ("src/user.js", &consumer),
            ]);
            let edges = resolve_references(&files, Language::JavaScript, "repo:test:abc");
            assert!(
                edges.iter().any(
                    |edge| edge.target_uid == uid(&files, "src/helper.js", "actual")
                        && edge.edge_type == EdgeType::Calls
                ),
                "valid {name} public name: {edges:#?}"
            );
        }
    }

    #[test]
    fn unresolved_bound_import_cannot_fall_back_to_a_sibling_homonym() {
        for importer in [
            "import { selected } from './missing.js';\nfunction user() {\n  selected();\n}\n",
            "import * as ns from './missing.js';\nfunction user() {\n  ns.selected();\n}\n",
        ] {
            let files = parsed_files(&[
                ("src/other.js", "export function selected() {}\n"),
                ("src/user.js", importer),
            ]);
            let edges = resolve_references(&files, Language::JavaScript, "repo:test:abc");
            assert!(
                !edges
                    .iter()
                    .any(|edge| edge.target_uid == uid(&files, "src/other.js", "selected")),
                "unresolved binding donated sibling: {edges:#?}"
            );
        }
        let files = parsed_files(&[(
            "src/user.js",
            "function selected() {}\nasync function owner() {\n  const { selected } = await import('./missing.js');\n  selected();\n}\nfunction outsider() {\n  selected();\n}\n",
        )]);
        let edges = resolve_references(&files, Language::JavaScript, "repo:test:abc");
        let target = uid(&files, "src/user.js", "selected");
        assert!(
            !edges.iter().any(
                |edge| edge.source_uid == uid(&files, "src/user.js", "owner")
                    && edge.target_uid == target
            ),
            "unresolved local binding must shadow the global function: {edges:#?}"
        );
        assert!(
            edges.iter().any(
                |edge| edge.source_uid == uid(&files, "src/user.js", "outsider")
                    && edge.target_uid == target
                    && edge.edge_type == EdgeType::Calls
            ),
            "local missing import must not escape into outsider: {edges:#?}"
        );
    }

    #[test]
    fn named_default_reexport_preserves_exact_default_target() {
        let files = parsed_files(&[
            (
                "src/Router.js",
                "export default function Router() {}\nexport function unused() {}\n",
            ),
            ("src/barrel.js", "export { default } from './Router.js';\n"),
            (
                "src/user.js",
                "import Router from './barrel.js';\nfunction user() {\n  Router();\n}\n",
            ),
        ]);
        let edges = resolve_references(&files, Language::JavaScript, "repo:test:abc");
        let target = uid(&files, "src/Router.js", "Router");
        for kind in [EdgeType::Calls, EdgeType::Imports] {
            assert!(
                edges
                    .iter()
                    .any(|edge| edge.source_uid == uid(&files, "src/user.js", "user")
                        && edge.target_uid == target
                        && edge.edge_type == kind),
                "{edges:#?}"
            );
        }
        assert!(
            !edges
                .iter()
                .any(|edge| edge.target_uid == uid(&files, "src/Router.js", "unused")),
            "{edges:#?}"
        );
    }

    #[test]
    fn imported_local_reexport_preserves_exact_original_target() {
        for (barrel, public) in [
            (
                "import { secret } from './target.js';\nexport { secret };\n",
                "secret",
            ),
            (
                "import { secret as selected } from './target.js';\nexport { selected as publicName };\n",
                "publicName",
            ),
        ] {
            let consumer = format!(
                "import {{ {public} as chosen }} from './a.js';\nfunction user() {{\n  chosen();\n}}\n"
            );
            let files = parsed_files(&[
                ("src/a.js", barrel),
                (
                    "src/target.js",
                    "export function secret() {}\nexport function unused() {}\n",
                ),
                ("src/user.js", &consumer),
            ]);
            let edges = resolve_references(&files, Language::JavaScript, "repo:test:abc");
            for kind in [EdgeType::Calls, EdgeType::Imports] {
                assert!(
                    edges
                        .iter()
                        .any(|edge| edge.source_uid == uid(&files, "src/user.js", "user")
                            && edge.target_uid == uid(&files, "src/target.js", "secret")
                            && edge.edge_type == kind),
                    "{barrel}: {edges:#?}"
                );
            }
            assert!(
                !edges
                    .iter()
                    .any(|edge| edge.target_uid == uid(&files, "src/target.js", "unused")),
                "{edges:#?}"
            );
        }
        let files = parsed_files(&[
            (
                "src/a.js",
                "function secret() {}\nasync function owner() {\n  const { secret } = await import('./target.js');\n  secret();\n}\nexport { secret };\n",
            ),
            ("src/target.js", "export function secret() {}\n"),
            (
                "src/user.js",
                "import { secret as chosen } from './a.js';\nfunction user() {\n  chosen();\n}\n",
            ),
        ]);
        let edges = resolve_references(&files, Language::JavaScript, "repo:test:abc");
        assert!(
            edges
                .iter()
                .any(|edge| edge.source_uid == uid(&files, "src/user.js", "user")
                    && edge.target_uid == uid(&files, "src/a.js", "secret")
                    && edge.edge_type == EdgeType::Calls),
            "function-local import must not replace the module export: {edges:#?}"
        );
        let files = parsed_files(&[
            (
                "src/a.js",
                "import { secret as selected } from './missing.js';\nexport { selected as publicName };\n",
            ),
            ("src/target.js", "export function secret() {}\n"),
            (
                "src/user.js",
                "import { publicName as chosen } from './a.js';\nfunction user() {\n  chosen();\n}\n",
            ),
        ]);
        let edges = resolve_references(&files, Language::JavaScript, "repo:test:abc");
        assert!(
            !edges
                .iter()
                .any(|edge| edge.target_uid == uid(&files, "src/target.js", "secret")),
            "missing forwarding source: {edges:#?}"
        );
    }

    #[test]
    fn correction_failed_binding_never_rebinds_to_unrelated_import() {
        for a in [
            "function secret() {}\nexport function marker() {}\n",
            "export function marker() {}\n",
        ] {
            let files = parsed_files(&[
                ("src/a.js", a),
                (
                    "src/b.js",
                    "export function secret() {}\nexport function other() {}\n",
                ),
                (
                    "src/user.js",
                    "import { secret as selected } from './a.js';\nimport { other } from './b.js';\nfunction user() {\n  selected();\n}\n",
                ),
            ]);
            let edges = resolve_references(&files, Language::JavaScript, "repo:test:abc");
            assert!(
                !edges
                    .iter()
                    .any(|edge| edge.target_uid == uid(&files, "src/b.js", "secret")),
                "{edges:#?}"
            );
        }
        let files = parsed_files(&[
            ("src/a.js", "export { secret } from './target.js';\n"),
            ("src/target.js", "export function secret() {}\n"),
            (
                "src/b.js",
                "export function secret() {}\nexport function other() {}\n",
            ),
            (
                "src/user.js",
                "import { secret as selected } from './a.js';\nimport { other } from './b.js';\nfunction user() {\n  selected();\n}\n",
            ),
        ]);
        let edges = resolve_references(&files, Language::JavaScript, "repo:test:abc");
        assert!(
            edges
                .iter()
                .any(|edge| edge.source_uid == uid(&files, "src/user.js", "user")
                    && edge.target_uid == uid(&files, "src/target.js", "secret")
                    && edge.edge_type == EdgeType::Calls),
            "{edges:#?}"
        );
        assert!(
            !edges
                .iter()
                .any(|edge| edge.target_uid == uid(&files, "src/b.js", "secret")),
            "{edges:#?}"
        );
    }

    #[test]
    fn correction_default_parameter_initializer_is_a_use_not_a_binding() {
        let files = parsed_files(&[
            ("src/helper.js", "export function helper() { return 1; }\n"),
            (
                "src/user.js",
                "import { helper } from './helper.js';\nfunction consume(fallback = helper()) {\n  return fallback;\n}\nfunction shadow(helper) {\n  helper();\n}\n",
            ),
        ]);
        let edges = resolve_references(&files, Language::JavaScript, "repo:test:abc");
        let target = uid(&files, "src/helper.js", "helper");
        assert!(
            edges.iter().any(
                |edge| edge.source_uid == uid(&files, "src/user.js", "consume")
                    && edge.target_uid == target
                    && edge.edge_type == EdgeType::Calls
            ),
            "{edges:#?}"
        );
        assert!(
            !edges.iter().any(
                |edge| edge.source_uid == uid(&files, "src/user.js", "shadow")
                    && edge.target_uid == target
            ),
            "{edges:#?}"
        );
    }

    #[test]
    fn correction_commonjs_object_exports_bind_exact_local_values_only() {
        let files = parsed_files(&[
            (
                "src/helper.js",
                "function used() {}\nfunction actual() {}\nfunction hidden() {}\nfunction bogus() {}\nmodule.exports = { used, renamed: actual, bogus: 42 };\n",
            ),
            (
                "src/user.js",
                "const { used, renamed: chosen } = require('./helper.js');\nfunction user() {\n  used();\n  chosen();\n}\n",
            ),
        ]);
        let edges = resolve_references(&files, Language::JavaScript, "repo:test:abc");
        for name in ["used", "actual"] {
            assert!(
                edges
                    .iter()
                    .any(|edge| edge.source_uid == uid(&files, "src/user.js", "user")
                        && edge.target_uid == uid(&files, "src/helper.js", name)
                        && edge.edge_type == EdgeType::Calls),
                "{name}: {edges:#?}"
            );
        }
        for name in ["hidden", "bogus"] {
            assert!(
                !edges
                    .iter()
                    .any(|edge| edge.target_uid == uid(&files, "src/helper.js", name)),
                "{name}: {edges:#?}"
            );
            assert_eq!(
                files[0]
                    .1
                    .iter()
                    .find(|symbol| symbol.name == name)
                    .unwrap()
                    .visibility,
                Visibility::Private
            );
        }
    }

    #[test]
    fn default_import_uses_the_exact_named_default_export() {
        for exporter in [
            "export function unused() {}\nexport default function Router() { return 1; }\n",
            "function Router() { return 1; }\nexport function unused() {}\nexport { Router as default };\n",
        ] {
            let files = parsed_files(&[
                ("src/Router.js", exporter),
                (
                    "src/App.js",
                    "import Selected from './Router.js';\nconst first = 0;\nfunction app() {\n  return Selected();\n}\nfunction unrelated() { return 1; }\n",
                ),
            ]);
            let edges = resolve_references(&files, Language::JavaScript, "repo:test:abc");
            let target = uid(&files, "src/Router.js", "Router");
            for kind in [EdgeType::Calls, EdgeType::Imports] {
                assert!(
                    edges
                        .iter()
                        .any(|edge| edge.source_uid == uid(&files, "src/App.js", "app")
                            && edge.target_uid == target
                            && edge.edge_type == kind),
                    "{exporter}: {edges:#?}"
                );
            }
            assert!(
                !edges
                    .iter()
                    .any(|edge| edge.target_uid == uid(&files, "src/Router.js", "unused")),
                "{edges:#?}"
            );
            assert!(
                !edges.iter().any(
                    |edge| edge.source_uid == uid(&files, "src/App.js", "unrelated")
                        && edge.edge_type == EdgeType::Imports
                ),
                "{edges:#?}"
            );
        }
    }

    #[test]
    fn default_namespace_member_targets_the_default_export() {
        let files = parsed_files(&[
            (
                "src/Router.js",
                "export function unused() {}\nexport default function Router() {}\n",
            ),
            (
                "src/App.js",
                "import * as ns from './Router.js';\nfunction app() {\n  ns.default();\n}\n",
            ),
        ]);
        let edges = resolve_references(&files, Language::JavaScript, "repo:test:abc");
        let target = uid(&files, "src/Router.js", "Router");
        assert!(
            edges
                .iter()
                .any(|edge| edge.source_uid == uid(&files, "src/App.js", "app")
                    && edge.target_uid == target
                    && edge.edge_type == EdgeType::Calls),
            "{edges:#?}"
        );
        assert!(
            !edges
                .iter()
                .any(|edge| edge.target_uid == uid(&files, "src/Router.js", "unused")),
            "{edges:#?}"
        );
    }

    #[test]
    fn type_imports_attribute_only_the_enclosing_signature_user() {
        let files = parsed_files(&[
            (
                "src/types.ts",
                "export interface Shape { value: number; }\nexport interface Unused { value: number; }\n",
            ),
            (
                "src/user.ts",
                "import type { Shape as Input, Unused } from './types';\nconst first = 1;\nfunction consume(input: Input) {\n  return input.value;\n}\nfunction unrelated() { return 0; }\n",
            ),
        ]);
        let edges = resolve_references(&files, Language::TypeScript, "repo:test:abc");
        let imports: Vec<_> = edges
            .iter()
            .filter(|edge| edge.edge_type == EdgeType::Imports)
            .map(|edge| (edge.source_uid.clone(), edge.target_uid.clone()))
            .collect();
        assert_eq!(
            imports,
            vec![(
                uid(&files, "src/user.ts", "consume"),
                uid(&files, "src/types.ts", "Shape")
            )],
            "{edges:#?}"
        );
    }

    #[test]
    fn imported_module_does_not_donate_private_or_unbound_homonyms() {
        let files = parsed_files(&[
            (
                "src/helper.js",
                "export function selected() { return 1; }\nexport function unbound() { return 2; }\nfunction secret() { return 3; }\n",
            ),
            (
                "src/consumer.js",
                "import { selected, secret } from './helper.js';\nfunction user() {\n  unbound();\n  secret();\n}\nfunction local() { return 0; }\n",
            ),
        ]);
        let edges = resolve_references(&files, Language::JavaScript, "repo:test:abc");
        for name in ["unbound", "secret"] {
            let target = uid(&files, "src/helper.js", name);
            assert!(
                !edges.iter().any(|edge| edge.target_uid == target),
                "unbound/private {name}: {edges:#?}"
            );
        }
    }

    #[test]
    fn function_local_import_binding_does_not_escape_and_namespace_can_be_shadowed() {
        let files = parsed_files(&[
            ("src/helper.js", "export function used() { return 1; }\n"),
            (
                "src/consumer.js",
                "import * as helper from './helper.js';\nasync function owner() {\n  const { used: selected } = await import('./helper.js');\n  selected();\n}\nfunction outsider() {\n  selected();\n}\nfunction shadow(helper) {\n  helper.used();\n}\n",
            ),
        ]);
        let edges = resolve_references(&files, Language::JavaScript, "repo:test:abc");
        let target = uid(&files, "src/helper.js", "used");
        assert!(
            edges.iter().any(
                |edge| edge.source_uid == uid(&files, "src/consumer.js", "owner")
                    && edge.target_uid == target
                    && edge.edge_type == EdgeType::Imports
            ),
            "{edges:#?}"
        );
        for name in ["outsider", "shadow"] {
            assert!(
                !edges.iter().any(
                    |edge| edge.source_uid == uid(&files, "src/consumer.js", name)
                        && edge.target_uid == target
                ),
                "{name}: {edges:#?}"
            );
        }
    }

    #[test]
    fn enclosed_dynamic_import_connects_only_the_used_named_member() {
        let files = parsed_files(&[
            (
                "src/helper.js",
                "export function unused() { return 0; }\nexport function used() { return 1; }\n",
            ),
            (
                "src/consumer.js",
                "const first = 0;\nasync function user() {\n  const { used: selected } = await import('./helper.js');\n  return selected();\n}\nasync function sideEffect() {\n  await import('./helper.js');\n}\n",
            ),
        ]);
        let edges = resolve_references(&files, Language::JavaScript, "repo:test:abc");
        let imports: Vec<_> = edges
            .iter()
            .filter(|edge| edge.edge_type == EdgeType::Imports)
            .map(|edge| (edge.source_uid.clone(), edge.target_uid.clone()))
            .collect();
        assert_eq!(
            imports,
            vec![(
                uid(&files, "src/consumer.js", "user"),
                uid(&files, "src/helper.js", "used")
            )],
            "{edges:#?}"
        );
    }

    #[test]
    fn parsed_swift_static_receiver_selects_its_declaring_type() {
        let files = parsed_files(&[(
            "Worker.swift",
            "struct Other {\n  static func execute() {}\n}\nstruct Worker {\n  static func execute() {}\n}\nfunc runner() {\n  Worker.execute()\n}\n",
        )]);
        let reference = files[0]
            .2
            .iter()
            .find(|reference| reference.kind == ReferenceKind::Call && reference.name == "execute")
            .unwrap();
        assert_eq!(
            reference.receiver.as_deref(),
            Some("Worker"),
            "{reference:?}"
        );
        let edges = resolve_references(&files, Language::Swift, "repo:test:abc");
        let worker = files[0]
            .1
            .iter()
            .find(|symbol| {
                symbol.name == "execute" && symbol.parent_name.as_deref() == Some("Worker")
            })
            .unwrap();
        let target = symbol_uid(
            "repo:test:abc",
            "Worker.swift",
            "execute",
            worker.start_line,
        );
        assert!(
            edges.iter().any(
                |edge| edge.source_uid == uid(&files, "Worker.swift", "runner")
                    && edge.target_uid == target
                    && edge.edge_type == EdgeType::Calls
            ),
            "{edges:#?}"
        );
        assert!(
            !edges.iter().any(
                |edge| edge.source_uid == uid(&files, "Worker.swift", "runner")
                    && edge.target_uid != target
                    && edge.edge_type == EdgeType::Calls
            ),
            "{edges:#?}"
        );
    }

    #[test]
    fn parsed_async_function_expression_owns_its_call_once() {
        for path in ["main.js", "main.ts"] {
            let files = parsed_files(&[(
                path,
                "function helper() {}\nconst work = async function() {\n  helper();\n};\nconst sync = function() { helper(); };\nconst arrow = async () => { helper(); };\nconst sync_arrow = () => { helper(); };\n",
            )]);
            for name in ["work", "sync", "arrow", "sync_arrow"] {
                let hits: Vec<_> = files[0]
                    .1
                    .iter()
                    .filter(|symbol| symbol.name == name)
                    .collect();
                assert_eq!(hits.len(), 1, "{path} duplicate {name}: {hits:?}");
                assert_eq!(hits[0].kind, SymbolKind::Function);
            }
            let work = files[0]
                .1
                .iter()
                .find(|symbol| symbol.name == "work")
                .unwrap();
            assert_eq!(
                (work.start_line, work.end_line),
                (2, 4),
                "{path}: async expression span must include its body"
            );
            let edges = resolve_references(
                &files,
                if path.ends_with(".ts") {
                    Language::TypeScript
                } else {
                    Language::JavaScript
                },
                "repo:test:abc",
            );
            for name in ["work", "sync", "arrow", "sync_arrow"] {
                let calls: Vec<_> = edges
                    .iter()
                    .filter(|edge| {
                        edge.edge_type == EdgeType::Calls
                            && edge.source_uid == uid(&files, path, name)
                    })
                    .collect();
                assert_eq!(
                    calls.len(),
                    1,
                    "{path}: {name} must own exactly its helper call: {calls:#?}"
                );
                assert_eq!(
                    calls[0].target_uid,
                    uid(&files, path, "helper"),
                    "{path}: {name} helper target"
                );
            }
        }
    }

    #[test]
    fn parsed_python_src_layout_connects_test_to_definition() {
        let files = parsed_files(&[
            ("src/qrec/coredata.py", "def load():\n    return 1\n"),
            (
                "tests/test_coredata.py",
                "from qrec.coredata import load\ndef test_load():\n    load()\n",
            ),
        ]);
        let edges = resolve_references(&files, Language::Python, "repo:test:abc");
        assert!(
            edges.iter().any(|edge| edge.edge_type == EdgeType::Calls
                && edge.source_uid == uid(&files, "tests/test_coredata.py", "test_load")
                && edge.target_uid == uid(&files, "src/qrec/coredata.py", "load")),
            "{edges:#?}"
        );
    }

    type ReviewParsedFiles = Vec<(String, Vec<RawSymbol>, Vec<RawReference>)>;

    fn review_edges(
        inputs: &[(&str, &str)],
        language: Language,
    ) -> (ReviewParsedFiles, Vec<ResolvedEdge>) {
        let mut files = Vec::new();
        let mut envs = std::collections::HashMap::new();
        for (path, source) in inputs {
            let parsed = parse_source(Path::new(path), source).unwrap();
            let env = crate::types::TypeEnvironment::build(
                source,
                language,
                &parsed.symbols,
                &parsed.type_bindings,
            );
            envs.insert((*path).to_string(), env);
            files.push(((*path).to_string(), parsed.symbols, parsed.references));
        }
        let edges = resolve_references_with_context(
            &files,
            language,
            "repo:test:abc",
            &WorkspaceContext::default(),
            Some(&envs),
            None,
        );
        (files, edges)
    }

    fn review_callers(
        files: &[(String, Vec<RawSymbol>, Vec<RawReference>)],
        edges: &[ResolvedEdge],
        path: &str,
        name: &str,
    ) -> std::collections::BTreeSet<String> {
        let target = uid(files, path, name);
        edges
            .iter()
            .filter(|edge| edge.edge_type == EdgeType::Calls && edge.target_uid == target)
            .map(|edge| edge.source_uid.clone())
            .collect()
    }

    #[test]
    fn review4_rust_module_impl_trait_and_nested_function_classification() {
        let path = "src/lib.rs";
        let source = "struct S;\nimpl S { fn method(&self) {} }\ntrait T { fn trait_method(&self) {} }\nmod tests { fn module_free() {} }\nfn enclosing() {\n fn nested_free() {}\n nested_free();\n}\n";
        let (files, edges) = review_edges(&[(path, source)], Language::Rust);
        for (name, kind) in [
            ("method", SymbolKind::Method),
            ("trait_method", SymbolKind::Method),
            ("module_free", SymbolKind::Function),
            ("nested_free", SymbolKind::Function),
        ] {
            let symbols: Vec<_> = files[0]
                .1
                .iter()
                .filter(|symbol| symbol.name == name)
                .collect();
            assert_eq!(symbols.len(), 1, "{name}: {symbols:#?}");
            assert_eq!(symbols[0].kind, kind, "{name}: {symbols:#?}");
        }
        assert_eq!(
            review_callers(&files, &edges, path, "nested_free"),
            std::collections::BTreeSet::from([uid(&files, path, "enclosing")]),
            "{edges:#?}"
        );
    }

    #[test]
    fn review4_rust_factory_same_name_uses_exact_lexical_declaration() {
        let path = "src/lib.rs";
        let source = r#"struct Right;
impl Right { fn right(&self) {} }
struct Wrong;
impl Wrong { fn wrong(&self) {} }
fn factory() -> Wrong { Wrong }
mod tests {
 use super::*;
 fn factory() -> Right { Right }
 fn caller() { let result = factory(); result.right(); result.wrong(); }
}
"#;
        let (files, edges) = review_edges(&[(path, source)], Language::Rust);
        assert_eq!(
            review_callers(&files, &edges, path, "right"),
            std::collections::BTreeSet::from([uid(&files, path, "caller")]),
            "{files:#?} {edges:#?}"
        );
        assert!(
            review_callers(&files, &edges, path, "wrong").is_empty(),
            "wrong factory donated owner: {edges:#?}"
        );
        let factory = files[0]
            .1
            .iter()
            .find(|symbol| symbol.name == "factory" && symbol.start_line > 6)
            .unwrap();
        assert!(
            edges.iter().any(|edge| edge.edge_type == EdgeType::Calls
                && edge.source_uid == uid(&files, path, "caller")
                && edge.target_uid
                    == symbol_uid("repo:test:abc", path, "factory", factory.start_line)),
            "exact callee edge: {edges:#?}"
        );
        assert!(
            !edges.iter().any(|edge| edge.edge_type == EdgeType::Calls
                && edge.source_uid == uid(&files, path, "caller")
                && edge.target_uid == uid(&files, path, "factory")),
            "outer callee must not win: {edges:#?}"
        );
    }

    #[test]
    fn review4_same_spelling_rust_classes_keep_module_owner() {
        let path = "src/lib.rs";
        let source = r#"struct T;
impl T { fn top(&self) {} }
mod hidden {
 pub struct T;
 impl T { pub fn nested_method(&self) {} }
 fn hidden_local(t: &T) { t.nested_method(); t.top(); }
}
fn top_user(t: &T) { t.top(); t.nested_method(); }
"#;
        let (files, edges) = review_edges(&[(path, source)], Language::Rust);
        assert_eq!(
            review_callers(&files, &edges, path, "top"),
            std::collections::BTreeSet::from([uid(&files, path, "top_user")]),
            "{edges:#?}"
        );
        assert_eq!(
            review_callers(&files, &edges, path, "nested_method"),
            std::collections::BTreeSet::from([uid(&files, path, "hidden_local")]),
            "{edges:#?}"
        );
    }

    #[test]
    fn review4_same_provider_type_aliases_keep_import_declaration() {
        let path = "src/consumer.rs";
        let source = r#"use crate::provider::First as Alias;
mod sibling {
 use crate::provider::Second as Alias;
 fn sibling_user(value: &Alias) { value.second(); value.first(); }
}
fn top_user(value: &Alias) { value.first(); value.second(); }
"#;
        let (files, edges) = review_edges(
            &[
                ("src/lib.rs", "mod consumer; mod provider;"),
                (path, source),
                (
                    "src/provider.rs",
                    r#"pub struct First;
impl First { pub fn first(&self) {} }
pub struct Second;
impl Second { pub fn second(&self) {} }
"#,
                ),
            ],
            Language::Rust,
        );
        assert_eq!(
            review_callers(&files, &edges, "src/provider.rs", "first"),
            std::collections::BTreeSet::from([uid(&files, path, "top_user")]),
            "{edges:#?}"
        );
        assert_eq!(
            review_callers(&files, &edges, "src/provider.rs", "second"),
            std::collections::BTreeSet::from([uid(&files, path, "sibling_user")]),
            "{edges:#?}"
        );
    }

    #[test]
    fn review4_factory_imports_keep_current_module_provenance() {
        for (top_import, nested_import, providers, top_target, nested_target) in [
            (
                "use crate::a::factory;",
                "use crate::b::factory;",
                vec![
                    (
                        "src/a.rs",
                        "pub struct First;\nimpl First { pub fn first(&self) {} }\npub fn factory() -> First { First }\n",
                    ),
                    (
                        "src/b.rs",
                        "pub struct Second;\nimpl Second { pub fn second(&self) {} }\npub fn factory() -> Second { Second }\n",
                    ),
                ],
                ("src/a.rs", "factory"),
                ("src/b.rs", "factory"),
            ),
            (
                "use crate::provider::first_factory as factory;",
                "use crate::provider::second_factory as factory;",
                vec![(
                    "src/provider.rs",
                    "pub struct First;\nimpl First { pub fn first(&self) {} }\npub struct Second;\nimpl Second { pub fn second(&self) {} }\npub fn first_factory() -> First { First }\npub fn second_factory() -> Second { Second }\n",
                )],
                ("src/provider.rs", "first_factory"),
                ("src/provider.rs", "second_factory"),
            ),
        ] {
            let source = format!(
                "{top_import}\nfn top_user() {{ let value = factory(); value.first(); value.second(); }}\nmod nested {{\n {nested_import}\n fn nested_user() {{ let value = factory(); value.second(); value.first(); }}\n}}\nmod outside {{ fn missing() {{ let value = factory(); value.first(); value.second(); }} }}\n"
            );
            let mut inputs = vec![
                ("src/lib.rs", "mod consumer; mod a; mod b; mod provider;"),
                ("src/consumer.rs", source.as_str()),
            ];
            inputs.extend(providers);
            let (files, edges) = review_edges(&inputs, Language::Rust);
            assert_eq!(
                review_callers(&files, &edges, top_target.0, "first"),
                std::collections::BTreeSet::from([uid(&files, "src/consumer.rs", "top_user")]),
                "{source}: {edges:#?}"
            );
            assert_eq!(
                review_callers(&files, &edges, nested_target.0, "second"),
                std::collections::BTreeSet::from([uid(&files, "src/consumer.rs", "nested_user")]),
                "{source}: {edges:#?}"
            );
            for (caller, target) in [("top_user", top_target), ("nested_user", nested_target)] {
                assert_eq!(
                    review_callers(&files, &edges, target.0, target.1),
                    std::collections::BTreeSet::from([uid(&files, "src/consumer.rs", caller)]),
                    "exact factory: {source}: {edges:#?}"
                );
            }
        }
    }

    #[test]
    fn review4_rust_exact_inline_type_import_retains_module_and_owner() {
        let (files, edges) = review_edges(
            &[
                (
                    "src/lib.rs",
                    "mod browser_ws; mod consumer;\npub use browser_ws::flow::ViewerFlow as PublicFlow;\n",
                ),
                (
                    "src/consumer.rs",
                    "use crate::PublicFlow;\nfn forwarded(viewer: &PublicFlow) { viewer.seed_unacked(); viewer.wrong_root(); viewer.wrong_hidden(); viewer.wrong_fn(); }\n",
                ),
                (
                    "src/browser_ws.rs",
                    r#"pub struct ViewerFlow;
impl ViewerFlow { pub fn wrong_root(&self) {} }
pub(crate) mod flow {
 pub(crate) struct ViewerFlow;
 impl ViewerFlow {
  pub(crate) fn new() -> Self { Self }
  pub(crate) fn seed_unacked(&self) {}
 }
 pub(crate) struct ReplayWindow;
 impl ReplayWindow {
  pub(crate) fn new() -> Self { Self }
  pub(crate) fn replay_tail(&self) {}
 }
 pub(crate) mod nested {
  pub(crate) struct ViewerFlow;
  impl ViewerFlow { pub(crate) fn nested_method(&self) {} }
 }
}
mod hidden {
 pub struct ViewerFlow;
 impl ViewerFlow { pub fn wrong_hidden(&self) {} }
}
fn local_type_donor() {
 struct ViewerFlow;
 impl ViewerFlow { pub fn wrong_fn(&self) {} }
 let viewer: ViewerFlow = ViewerFlow;
 viewer.wrong_fn();
}
mod tests {
 use crate::browser_ws::flow::{ViewerFlow, ReplayWindow};
 fn annotated(viewer: &ViewerFlow, window: &ReplayWindow) {
  viewer.seed_unacked(); window.replay_tail(); viewer.wrong_root(); viewer.wrong_hidden(); viewer.wrong_fn();
 }
 fn make_flow() -> ViewerFlow { ViewerFlow::new() }
 fn make_window() -> ReplayWindow { ReplayWindow::new() }
 fn inferred() {
  let viewer = make_flow(); let window = make_window();
  viewer.seed_unacked(); window.replay_tail(); viewer.wrong_root(); viewer.wrong_hidden(); viewer.wrong_fn();
 }
 fn constructor() {
  let viewer = ViewerFlow::new(); let window = ReplayWindow::new();
  viewer.seed_unacked(); window.replay_tail(); viewer.wrong_root(); viewer.wrong_hidden(); viewer.wrong_fn();
 }
 mod nested_alias {
  use crate::browser_ws::flow::nested::ViewerFlow as Nested;
  fn nested_user(viewer: &Nested) { viewer.nested_method(); viewer.seed_unacked(); viewer.wrong_root(); }
 }
 mod unresolved {
  use crate::browser_ws::missing::ViewerFlow;
  fn unresolved_user(viewer: &ViewerFlow) { viewer.seed_unacked(); viewer.wrong_root(); }
 }
 mod glob {
  use crate::browser_ws::flow::*;
  fn unsupported_glob(viewer: &ViewerFlow) { viewer.seed_unacked(); }
 }
 fn unknown(viewer: Unknown) { viewer.seed_unacked(); }
}
"#,
                ),
            ],
            Language::Rust,
        );
        let mut seed_callers = std::collections::BTreeSet::from([
            uid(&files, "src/browser_ws.rs", "annotated"),
            uid(&files, "src/browser_ws.rs", "inferred"),
            uid(&files, "src/browser_ws.rs", "constructor"),
        ]);
        assert_eq!(
            review_callers(&files, &edges, "src/browser_ws.rs", "replay_tail"),
            seed_callers,
            "{edges:#?}"
        );
        seed_callers.insert(uid(&files, "src/consumer.rs", "forwarded"));
        assert_eq!(
            review_callers(&files, &edges, "src/browser_ws.rs", "seed_unacked"),
            seed_callers,
            "{edges:#?}"
        );
        assert_eq!(
            review_callers(&files, &edges, "src/browser_ws.rs", "nested_method"),
            std::collections::BTreeSet::from([uid(&files, "src/browser_ws.rs", "nested_user")]),
            "{edges:#?}"
        );
        assert_eq!(
            review_callers(&files, &edges, "src/browser_ws.rs", "wrong_fn"),
            std::collections::BTreeSet::from([uid(
                &files,
                "src/browser_ws.rs",
                "local_type_donor"
            )]),
            "{edges:#?}"
        );
        for method in ["wrong_root", "wrong_hidden"] {
            assert!(
                review_callers(&files, &edges, "src/browser_ws.rs", method).is_empty(),
                "{edges:#?}"
            );
        }
    }

    #[test]
    fn review4_rust_private_parent_type_retains_exact_method_owner() {
        let (files, edges) = review_edges(
            &[
                (
                    "src/lib.rs",
                    "mod consumer;\nstruct Repository;\nimpl Repository { pub fn inspect(&self) {} }\nmod hidden {\n struct Repository;\n impl Repository { pub fn hidden_method(&self) {} }\n fn hidden_local(repo: &Repository) { repo.hidden_method(); repo.inspect(); }\n}\n",
                ),
                (
                    "src/consumer.rs",
                    "use super::Repository;\nfn immutable(repo: &Repository) { repo.inspect(); repo.hidden_method(); }\nfn mutable(repo: &mut Repository) { repo.inspect(); repo.hidden_method(); }\nfn make() -> Repository { panic!() }\nfn inferred() { let repo = make(); repo.inspect(); repo.hidden_method(); }\n",
                ),
            ],
            Language::Rust,
        );
        assert_eq!(
            review_callers(&files, &edges, "src/lib.rs", "inspect"),
            std::collections::BTreeSet::from([
                uid(&files, "src/consumer.rs", "immutable"),
                uid(&files, "src/consumer.rs", "mutable"),
                uid(&files, "src/consumer.rs", "inferred")
            ]),
            "{edges:#?}"
        );
        assert_eq!(
            review_callers(&files, &edges, "src/lib.rs", "hidden_method"),
            std::collections::BTreeSet::from([uid(&files, "src/lib.rs", "hidden_local")]),
            "{edges:#?}"
        );
    }

    #[test]
    fn review4_rust_typed_receiver_follows_only_exact_public_reexports() {
        for (facade, other, target) in [
            ("pub use crate::db::Repository;", "", Some("src/db.rs")),
            (
                "pub use crate::db::Original as Repository;",
                "",
                Some("src/db.rs"),
            ),
            (
                "pub use crate::other::Repository;",
                "pub use crate::db::Repository;",
                Some("src/db.rs"),
            ),
            (
                "pub use crate::donor::Repository;",
                "",
                Some("src/donor.rs"),
            ),
            ("use crate::db::Repository;", "", None),
            ("pub(in crate::facade) use crate::db::Repository;", "", None),
            ("mod hidden { pub use crate::db::Repository; }", "", None),
            ("pub use crate::missing::Repository;", "", None),
            (
                "pub use crate::db::Repository;\npub use crate::donor::Repository;",
                "",
                None,
            ),
            (
                "pub use crate::other::Repository;",
                "pub use crate::facade::Repository;",
                None,
            ),
        ] {
            let (files, edges) = review_edges(
                &[
                    (
                        "src/lib.rs",
                        "mod db; mod donor; mod consumer; mod facade; mod other;\npub use facade::Repository;",
                    ),
                    ("src/facade.rs", facade),
                    ("src/other.rs", other),
                    (
                        "src/db.rs",
                        "pub struct Repository;\nimpl Repository { pub fn inspect(&self) {} }\npub struct Original;\nimpl Original { pub fn inspect_original(&self) {} }\nmod hidden {\n pub struct Repository;\n impl Repository { pub fn hidden_repository(&self) {} }\n pub struct Original;\n impl Original { pub fn hidden_original(&self) {} }\n}\n",
                    ),
                    (
                        "src/donor.rs",
                        "pub struct Repository;\nimpl Repository { pub fn inspect(&self) {} }\n",
                    ),
                    (
                        "src/consumer.rs",
                        "use crate::{Repository};\nfn immutable(repo: &Repository) { repo.inspect(); repo.inspect_original(); repo.hidden_repository(); repo.hidden_original(); }\nfn mutable(repo: &mut Repository) { repo.inspect(); repo.inspect_original(); repo.hidden_repository(); repo.hidden_original(); }\nfn make() -> Repository { panic!() }\nfn inferred() { let repo = make(); repo.inspect(); repo.inspect_original(); repo.hidden_repository(); repo.hidden_original(); }\nmod hidden { pub struct Repository; }\n",
                    ),
                ],
                Language::Rust,
            );
            for method in ["hidden_repository", "hidden_original"] {
                assert!(
                    review_callers(&files, &edges, "src/db.rs", method).is_empty(),
                    "{facade}: {edges:#?}"
                );
            }
            let callers = std::collections::BTreeSet::from([
                uid(&files, "src/consumer.rs", "immutable"),
                uid(&files, "src/consumer.rs", "mutable"),
                uid(&files, "src/consumer.rs", "inferred"),
            ]);
            for (file, method) in [
                ("src/db.rs", "inspect"),
                ("src/db.rs", "inspect_original"),
                ("src/donor.rs", "inspect"),
            ] {
                let expected = if target == Some(file)
                    && (method == "inspect_original") == facade.contains("Original as Repository")
                {
                    callers.clone()
                } else {
                    Default::default()
                };
                assert_eq!(
                    review_callers(&files, &edges, file, method),
                    expected,
                    "{facade}: {edges:#?}"
                );
            }
        }
    }

    #[test]
    fn review4_rust_item_matching_module_filename_keeps_exact_factory() {
        let (files, edges) = review_edges(
            &[
                ("src/lib.rs", "mod consumer; mod factory; mod invalid;"),
                (
                    "src/consumer.rs",
                    r#"use crate::factory::factory;
fn bare_user() { let value = factory(); value.work(); }
mod namespace {
 use crate::factory;
 fn module_user() { factory::factory(); }
 fn unknown_receiver(factory: Unknown) { factory.factory(); }
}
"#,
                ),
                (
                    "src/invalid.rs",
                    "use crate::factory::factory;\nfn invalid_value_path() { factory::factory(); }\nfn invalid_method_path() { factory::work(); }\nfn invalid_value_dot() { factory.work(); }\n",
                ),
                (
                    "src/factory.rs",
                    "pub struct Service;\nimpl Service { pub fn work(&self) {} }\npub fn factory() -> Service { Service }\n",
                ),
            ],
            Language::Rust,
        );
        assert_eq!(
            review_callers(&files, &edges, "src/factory.rs", "work"),
            std::collections::BTreeSet::from([uid(&files, "src/consumer.rs", "bare_user")]),
            "{edges:#?}"
        );
        assert_eq!(
            review_callers(&files, &edges, "src/factory.rs", "factory"),
            std::collections::BTreeSet::from([
                uid(&files, "src/consumer.rs", "bare_user"),
                uid(&files, "src/consumer.rs", "module_user")
            ]),
            "{edges:#?}"
        );
    }

    #[test]
    fn review4_plain_rust_module_import_keeps_qualified_free_call() {
        let (files, edges) = review_edges(
            &[
                ("src/lib.rs", "mod consumer; mod provider;"),
                (
                    "src/consumer.rs",
                    "use crate::provider;\nfn user() { provider::factory(); }\nfn shadow(provider: Unknown) { provider.factory(); }\n",
                ),
                ("src/provider.rs", "pub fn factory() {}\n"),
            ],
            Language::Rust,
        );
        assert_eq!(
            review_callers(&files, &edges, "src/provider.rs", "factory"),
            std::collections::BTreeSet::from([uid(&files, "src/consumer.rs", "user")]),
            "{edges:#?}"
        );
    }

    #[test]
    fn review4_function_local_type_retains_exact_impl_owner() {
        let path = "src/lib.rs";
        let source = r#"struct T;
impl T { fn top(&self) {} }
fn local_user() {
 struct T;
 impl T { fn local(&self) {} }
 let value = T;
 value.local(); value.top();
}
fn top_user(value: &T) { value.top(); value.local(); }
"#;
        let (files, edges) = review_edges(&[(path, source)], Language::Rust);
        assert_eq!(
            review_callers(&files, &edges, path, "local"),
            std::collections::BTreeSet::from([uid(&files, path, "local_user")]),
            "{edges:#?}"
        );
        assert_eq!(
            review_callers(&files, &edges, path, "top"),
            std::collections::BTreeSet::from([uid(&files, path, "top_user")]),
            "{edges:#?}"
        );
    }

    #[test]
    fn review4_same_line_module_imports_keep_byte_identity() {
        let provider = "pub struct First;\nimpl First { pub fn first(&self) {} }\npub struct Second;\nimpl Second { pub fn second(&self) {} }\npub fn first_factory() -> First { First }\npub fn second_factory() -> Second { Second }\n";
        for source in [
            "use crate::provider::First as Alias; mod nested { use crate::provider::Second as Alias;\n fn nested_user(value: &Alias) { value.second(); value.first(); }\n}\nfn top_user(value: &Alias) { value.first(); value.second(); }\n",
            "use crate::provider::first_factory as factory; mod nested { use crate::provider::second_factory as factory;\n fn nested_user() { let value = factory(); value.second(); value.first(); }\n}\nfn top_user() { let value = factory(); value.first(); value.second(); }\n",
        ] {
            let (files, edges) = review_edges(
                &[
                    ("src/lib.rs", "mod consumer; mod provider;"),
                    ("src/consumer.rs", source),
                    ("src/provider.rs", provider),
                ],
                Language::Rust,
            );
            assert_eq!(
                review_callers(&files, &edges, "src/provider.rs", "first"),
                std::collections::BTreeSet::from([uid(&files, "src/consumer.rs", "top_user")]),
                "{source}: {edges:#?}"
            );
            assert_eq!(
                review_callers(&files, &edges, "src/provider.rs", "second"),
                std::collections::BTreeSet::from([uid(&files, "src/consumer.rs", "nested_user")]),
                "{source}: {edges:#?}"
            );
        }
    }

    #[test]
    fn review4_exact_imported_factory_survives_hidden_same_name_functions() {
        let path = "src/consumer.rs";
        let source = r#"struct Wrong;
impl Wrong { fn work(&self) {} }
fn factory() -> Wrong { Wrong }
mod hidden { fn factory() -> super::Wrong { todo!() } }
mod tests {
 use crate::provider::factory;
 fn imported_user() { let value = factory(); value.work(); }
 fn shadow(factory: Unknown) { let value = factory(); value.work(); }
}
mod outside { fn missing() { let value = factory(); value.work(); } }
"#;
        let (files, edges) = review_edges(
            &[
                ("src/lib.rs", "mod consumer; mod provider;"),
                (path, source),
                (
                    "src/provider.rs",
                    "pub struct ExternalType;\nimpl ExternalType { pub fn work(&self) {} }\npub fn factory() -> ExternalType { ExternalType }\n",
                ),
            ],
            Language::Rust,
        );
        assert_eq!(
            review_callers(&files, &edges, "src/provider.rs", "work"),
            std::collections::BTreeSet::from([uid(&files, path, "imported_user")]),
            "{files:#?} {edges:#?}"
        );
        assert!(
            review_callers(&files, &edges, path, "work").is_empty(),
            "hidden function must not donate Wrong: {edges:#?}"
        );
        assert!(
            edges.iter().any(|edge| edge.edge_type == EdgeType::Calls
                && edge.source_uid == uid(&files, path, "imported_user")
                && edge.target_uid == uid(&files, "src/provider.rs", "factory")),
            "exact external factory: {edges:#?}"
        );
    }

    #[test]
    fn review4_imported_annotation_and_return_refuse_hidden_local_type_donation() {
        let path = "src/consumer.rs";
        let source = r#"use crate::provider::ExternalType;
mod hidden {
 struct ExternalType;
 impl ExternalType { fn work(&self) {} }
 fn local(value: &ExternalType) { value.work(); }
}
fn annotated(value: &ExternalType) { value.work(); }
fn imported_return() -> ExternalType { todo!() }
fn returned() { let value = imported_return(); value.work(); }
fn shadow(value: &ExternalType) { let value = unknown(); value.work(); }
mod missing {
 use missing_provider::*;
 fn absent(value: &ExternalType) { value.work(); }
}
"#;
        let (files, edges) = review_edges(
            &[
                ("src/lib.rs", "mod consumer; mod provider;"),
                (path, source),
                (
                    "src/provider.rs",
                    "pub struct ExternalType;\nimpl ExternalType { pub fn work(&self) {} }\n",
                ),
            ],
            Language::Rust,
        );
        assert_eq!(
            review_callers(&files, &edges, "src/provider.rs", "work"),
            ["annotated", "returned"]
                .into_iter()
                .map(|name| uid(&files, path, name))
                .collect(),
            "{files:#?} {edges:#?}"
        );
        assert_eq!(
            review_callers(&files, &edges, path, "work"),
            std::collections::BTreeSet::from([uid(&files, path, "local")]),
            "hidden type only owns its actual local parameter: {edges:#?}"
        );
    }

    #[test]
    fn review4_rust_inline_module_factories_and_unit_constructors_keep_exact_methods() {
        let path = "src/lib.rs";
        let source = r#"struct KeyTr3;
impl KeyTr3 { fn seed_unacked(&self) {} }
fn top_helper() -> KeyTr3 { KeyTr3 }
mod tests {
 use super::*;
 fn nested_helper() -> KeyTr3 { KeyTr3 }
 fn nested_factory_user() { let t = nested_helper(); t.seed_unacked(); }
 fn unit_user() { let t = KeyTr3; t.seed_unacked(); }
 fn annotated_control() { let t: KeyTr3 = KeyTr3; t.seed_unacked(); }
 fn outer_factory_control() { let t = top_helper(); t.seed_unacked(); }
 fn local_shadow(KeyTr3: Unknown) { let t = KeyTr3; t.seed_unacked(); }
 fn factory_shadow(nested_helper: Unknown) { let t = nested_helper(); t.seed_unacked(); }
 fn receiver_shadow() { let t = nested_helper(); { let t = unknown(); t.seed_unacked(); } }
}
mod unresolved {
 use missing::*;
 fn invalid() { let t = KeyTr3; t.seed_unacked(); }
}
"#;
        let (files, edges) = review_edges(
            &[
                (path, source),
                (
                    "other/lib.rs",
                    "struct Other;\nimpl Other { fn seed_unacked(&self) {} }\n",
                ),
            ],
            Language::Rust,
        );
        assert_eq!(
            review_callers(&files, &edges, path, "seed_unacked"),
            [
                "nested_factory_user",
                "unit_user",
                "annotated_control",
                "outer_factory_control"
            ]
            .into_iter()
            .map(|name| uid(&files, path, name))
            .collect(),
            "{files:#?} {edges:#?}"
        );
        assert!(
            review_callers(&files, &edges, "other/lib.rs", "seed_unacked").is_empty(),
            "wrong owner: {edges:#?}"
        );
        for name in ["nested_helper", "nested_factory_user"] {
            let symbol = files[0].1.iter().find(|s| s.name == name).unwrap();
            assert_eq!(
                symbol.kind,
                SymbolKind::Function,
                "module member is a free function: {symbol:#?}"
            );
        }
        let method = files[0]
            .1
            .iter()
            .find(|s| s.name == "seed_unacked")
            .unwrap();
        assert_eq!(method.kind, SymbolKind::Method);
    }

    #[test]
    fn review4_rust_exact_member_return_keeps_nested_module_paths() {
        let path = "src/lib.rs";
        let source = r#"struct OrbitPaths;
impl OrbitPaths { fn create_private_dirs(&self) {} }
mod tests {
 use super::*;
 struct TempTree;
 impl TempTree {
  fn new() -> Self { TempTree }
  fn paths(&self) -> OrbitPaths { OrbitPaths }
 }
 fn caller() { let tree = TempTree::new(); let paths = tree.paths(); paths.create_private_dirs(); }
 fn wrong(tree: Unknown) { let paths = tree.paths(); paths.create_private_dirs(); }
 fn shadow() { let tree = TempTree::new(); let tree = unknown(); let paths = tree.paths(); paths.create_private_dirs(); }
}
"#;
        let (files, edges) = review_edges(&[(path, source)], Language::Rust);
        assert_eq!(
            review_callers(&files, &edges, path, "create_private_dirs"),
            std::collections::BTreeSet::from([uid(&files, path, "caller")]),
            "{files:#?} {edges:#?}"
        );
    }

    #[test]
    fn review4_js_implicit_constructor_field_is_exact_and_shadow_safe() {
        let path = "src/service.js";
        let source = r#"class Store { query() {} }
class Controller {
 constructor() { this.store = new Store(); }
 run() { this.store.query(); }
}
class UnknownController {
 constructor() { this.store = unknown(); }
 runUnknown() { this.store.query(); }
}
class ChangedController {
 constructor() { this.store = new Store(); this.store = unknown(); }
 runChanged() { this.store.query(); }
}
class ShadowController {
 constructor(Store) { this.store = new Store(); }
 runShadow() { this.store.query(); }
}
class CallbackController {
 constructor() { function callback() { this.store = new Store(); } }
 runCallback() { this.store.query(); }
}
"#;
        let (files, edges) = review_edges(&[(path, source)], Language::JavaScript);
        assert_eq!(
            review_callers(&files, &edges, path, "query"),
            std::collections::BTreeSet::from([uid(&files, path, "run")]),
            "{files:#?} {edges:#?}"
        );
    }

    #[test]
    fn review4_commonjs_literal_method_does_not_export_same_named_free_function() {
        let path = "src/provider.js";
        let (files, edges) = review_edges(
            &[
                (
                    path,
                    "function run() {}\nmodule.exports = {\n run() {}\n};\n",
                ),
                (
                    "src/user.js",
                    "const api = require('./provider');\nfunction user() { api.run(); }\n",
                ),
            ],
            Language::JavaScript,
        );
        let free = files[0]
            .1
            .iter()
            .find(|symbol| symbol.name == "run" && symbol.start_line == 1)
            .expect("actual free declaration");
        let method = files[0]
            .1
            .iter()
            .find(|symbol| symbol.name == "run" && symbol.start_line == 3)
            .expect("actual literal method");
        assert_eq!(free.visibility, Visibility::Private);
        assert!(!free.is_entry_point);
        assert_eq!(method.parent_name.as_deref(), Some("module.exports"));
        let target = symbol_uid("repo:test:abc", path, &method.name, method.start_line);
        let caller = uid(&files, "src/user.js", "user");
        let calls: Vec<_> = edges
            .iter()
            .filter(|edge| edge.edge_type == EdgeType::Calls && edge.source_uid == caller)
            .collect();
        assert_eq!(
            calls.len(),
            1,
            "nonvacuous exact call: {files:#?} {edges:#?}"
        );
        assert_eq!(
            calls[0].target_uid, target,
            "literal declaration identity: {edges:#?}"
        );
    }

    #[test]
    fn review4_commonjs_literal_method_does_not_forward_same_named_import() {
        let (files, edges) = review_edges(
            &[
                ("src/other.js", "export function run() {}\n"),
                (
                    "src/provider.js",
                    "import { run } from './other';\nmodule.exports = {\n run() {}\n};\n",
                ),
                (
                    "src/missing_provider.js",
                    "import { run } from './missing';\nmodule.exports = {\n run() {}\n};\n",
                ),
                (
                    "src/forward.js",
                    "import { run } from './other'; module.exports = { run };\n",
                ),
                (
                    "src/user.js",
                    "const good = require('./provider'); const missing = require('./missing_provider'); const forwarded = require('./forward');\nfunction good_user() { good.run(); }\nfunction missing_user() { missing.run(); }\nfunction forwarding_control() { forwarded.run(); }\n",
                ),
            ],
            Language::JavaScript,
        );
        for (caller_name, path, line) in [
            ("good_user", "src/provider.js", 3),
            ("missing_user", "src/missing_provider.js", 3),
            ("forwarding_control", "src/other.js", 1),
        ] {
            let caller = uid(&files, "src/user.js", caller_name);
            let calls: Vec<_> = edges
                .iter()
                .filter(|edge| edge.edge_type == EdgeType::Calls && edge.source_uid == caller)
                .collect();
            assert_eq!(
                calls.len(),
                1,
                "nonvacuous {caller_name}: {files:#?} {edges:#?}"
            );
            assert_eq!(
                calls[0].target_uid,
                symbol_uid("repo:test:abc", path, "run", line),
                "{caller_name}: {edges:#?}"
            );
        }
        let decoy = symbol_uid("repo:test:abc", "src/other.js", "run", 1);
        for name in ["good_user", "missing_user"] {
            let caller = uid(&files, "src/user.js", name);
            assert!(!edges.iter().any(|edge| edge.edge_type == EdgeType::Calls
                && edge.source_uid == caller
                && edge.target_uid == decoy));
        }
    }

    #[test]
    fn review4_commonjs_literal_identity_survives_aliases_and_other_method_owners() {
        let (files, edges) = review_edges(
            &[
                (
                    "src/a.js",
                    "function run() {}\nexport class Keeper { run() {} }\nmodule.exports = {\n run() {},\n free: run\n};\n",
                ),
                ("src/b.js", "module.exports = {\n run() {}\n};\n"),
                ("src/barrel.js", "export { run as execute } from './a';\n"),
                (
                    "src/user.js",
                    "const a = require('./a'); const b = require('./b');\nimport { execute } from './barrel';\nfunction first() { a.run(); }\nfunction second() { b.run(); }\nfunction forwarded() { execute(); }\nfunction identifier() { a.free(); }\nfunction unknown(a) { a.run(); }\n",
                ),
            ],
            Language::JavaScript,
        );
        for (caller_name, path, line) in [
            ("first", "src/a.js", 4),
            ("second", "src/b.js", 2),
            ("forwarded", "src/a.js", 4),
            ("identifier", "src/a.js", 1),
        ] {
            let caller = uid(&files, "src/user.js", caller_name);
            let calls: Vec<_> = edges
                .iter()
                .filter(|edge| edge.edge_type == EdgeType::Calls && edge.source_uid == caller)
                .collect();
            assert_eq!(calls.len(), 1, "{caller_name}: {files:#?} {edges:#?}");
            assert_eq!(
                calls[0].target_uid,
                symbol_uid("repo:test:abc", path, "run", line),
                "{caller_name}: {edges:#?}"
            );
        }
        let unknown = uid(&files, "src/user.js", "unknown");
        assert!(
            !edges.iter().any(|edge| edge.edge_type == EdgeType::Calls
                && edge.source_uid == unknown
                && edge.target_uid.starts_with("sym:")),
            "{edges:#?}"
        );
    }

    #[test]
    fn review4_commonjs_direct_function_assignments_and_class_exports_remain_supported() {
        let (files, edges) = review_edges(
            &[
                (
                    "src/direct.js",
                    "module.exports.run = function() {};\nexports.other = () => {};\nclass Thing {}\nmodule.exports.Thing = Thing;\n",
                ),
                (
                    "src/user.js",
                    "const api = require('./direct'); const { Thing } = require('./direct');\nfunction first() { api.run(); }\nfunction second() { api.other(); }\nfunction construct() { new Thing(); }\n",
                ),
            ],
            Language::JavaScript,
        );
        for (caller_name, target_name, line) in [
            ("first", "run", 1),
            ("second", "other", 2),
            ("construct", "Thing", 3),
        ] {
            let caller = uid(&files, "src/user.js", caller_name);
            let calls: Vec<_> = edges
                .iter()
                .filter(|edge| edge.edge_type == EdgeType::Calls && edge.source_uid == caller)
                .collect();
            assert_eq!(calls.len(), 1, "{caller_name}: {edges:#?}");
            assert_eq!(
                calls[0].target_uid,
                symbol_uid("repo:test:abc", "src/direct.js", target_name, line),
                "{edges:#?}"
            );
        }
    }

    #[test]
    fn review4_duplicate_literal_export_records_refuse_without_overwrite_order_guessing() {
        let (files, edges) = review_edges(
            &[
                (
                    "src/provider.js",
                    "module.exports = {\n run() {}\n};\nmodule.exports = {\n run() {}\n};\n",
                ),
                (
                    "src/user.js",
                    "const api = require('./provider'); function user() { api.run(); }",
                ),
            ],
            Language::JavaScript,
        );
        let caller = uid(&files, "src/user.js", "user");
        let targets: std::collections::HashSet<_> = files[0]
            .1
            .iter()
            .map(|symbol| {
                symbol_uid(
                    "repo:test:abc",
                    "src/provider.js",
                    &symbol.name,
                    symbol.start_line,
                )
            })
            .collect();
        assert!(
            !edges.iter().any(|edge| edge.edge_type == EdgeType::Calls
                && edge.source_uid == caller
                && targets.contains(&edge.target_uid)),
            "{edges:#?}"
        );
    }

    #[test]
    fn review4_commonjs_literal_method_is_an_exact_export() {
        let (files, edges) = review_edges(
            &[
                (
                    "src/provider.js",
                    "module.exports = { run(module) {}, get value() { return 1; } };\nconst decoy = { hidden() {} };\nfunction shadowModule(module) {}\n",
                ),
                (
                    "src/user.js",
                    "const api = require('./provider');\nfunction user() { api.run(); }\nfunction unknown(api) { api.run(); }\nfunction hidden() { api.hidden(); }\nfunction getter() { api.value(); }\n",
                ),
            ],
            Language::JavaScript,
        );
        assert_eq!(
            review_callers(&files, &edges, "src/provider.js", "run"),
            std::collections::BTreeSet::from([uid(&files, "src/user.js", "user")]),
            "{files:#?} {edges:#?}"
        );
        for name in ["hidden", "value"] {
            assert!(
                review_callers(&files, &edges, "src/provider.js", name).is_empty(),
                "{name}: {edges:#?}"
            );
        }
        let run = files[0]
            .1
            .iter()
            .find(|symbol| symbol.name == "run")
            .unwrap();
        assert_eq!(run.parent_name.as_deref(), Some("module.exports"));
        assert_eq!(run.visibility, Visibility::Public);
        assert!(run.is_entry_point);
    }

    #[test]
    fn review4_commonjs_literal_method_requires_exact_unshadowed_top_level_export() {
        for source in [
            "const ordinary = { run() {} };",
            "function later() { module.exports = { run() {} }; }",
            "module.exports = { ['run']() {} };",
            "const module = { exports: null }; module.exports = { run() {} };",
            "module.exports = { run() {} }; const module = { exports: null };",
            "module.exports = { run() {} }; if (false) { var module; }",
        ] {
            let (files, edges) = review_edges(
                &[
                    ("src/provider.js", source),
                    (
                        "src/user.js",
                        "const api = require('./provider'); function user() { api.run(); }",
                    ),
                ],
                Language::JavaScript,
            );
            let provider_uids: std::collections::HashSet<_> = files[0]
                .1
                .iter()
                .map(|symbol| {
                    symbol_uid(
                        "repo:test:abc",
                        "src/provider.js",
                        &symbol.name,
                        symbol.start_line,
                    )
                })
                .collect();
            assert!(
                !edges.iter().any(|edge| edge.edge_type == EdgeType::Calls
                    && provider_uids.contains(&edge.target_uid)),
                "{source}: {edges:#?}"
            );
        }
    }

    #[test]
    fn review2_macro_tuple_receiver_shadow_uses_its_lexical_declaration() {
        let path = "src/library.rs";
        let source = r#"use std::sync::Arc;
struct Storage;
impl Storage { pub fn create_nonce(&self) -> bool { true } }
fn factory() -> ((), Storage) { todo!() }
fn inner() {
 let (_, db) = factory();
 { let db = unknown_factory(); assert!(db.create_nonce()); }
}
fn after() {
 let (_, db) = factory();
 { let db = unknown_factory(); }
 assert!(db.create_nonce());
}
fn sibling() {
 let (_, db) = factory();
 { let db = unknown_factory(); }
 { assert!(db.create_nonce()); }
}
"#;
        let (files, edges) = review_edges(&[(path, source)], Language::Rust);
        assert_eq!(
            review_callers(&files, &edges, path, "create_nonce"),
            ["after", "sibling"]
                .into_iter()
                .map(|name| uid(&files, path, name))
                .collect(),
            "macro scopes must not borrow an unknown inner declaration's outer type: {files:#?} {edges:#?}"
        );
    }

    #[test]
    fn review2_rust_simple_typed_alias_keeps_its_scoped_source_type() {
        let path = "src/library.rs";
        let source = r#"struct Storage;
impl Storage { pub fn create_nonce(&self) {} }
fn user(db: &Storage) {
 let alias = db;
 alias.create_nonce();
}
fn unknown(db: &Storage) {
 let db = unknown_factory();
 let alias = db;
 alias.create_nonce();
}
"#;
        let (files, edges) = review_edges(&[(path, source)], Language::Rust);
        assert_eq!(
            review_callers(&files, &edges, path, "create_nonce"),
            std::collections::BTreeSet::from([uid(&files, path, "user")]),
            "simple alias preserves its active typed source; unknown shadow cannot donate it: {files:#?} {edges:#?}"
        );
    }

    #[test]
    fn review2_rust_tuple_factory_receivers_are_scoped_and_typed() {
        let path = "src/library.rs";
        let source = r#"use std::sync::Arc;
struct Storage;
impl Storage {
 pub fn create_nonce(&self) {}
 pub fn len(&self) -> usize { 1 }
 pub fn contains(&self) -> bool { true }
}
struct Registry;
impl Registry { pub fn register(&self) {} }
async fn spawn_server() -> (String, Arc<Storage>, Registry) { todo!() }
async fn user() {
 let (_, db, registry) = spawn_server().await;
 db.create_nonce();
 registry.register();
}
async fn inner_unknown() {
 let (_, db, _) = spawn_server().await;
 { let db = unknown_factory(); db.create_nonce(); }
}
async fn after_inner() {
 let (_, db, _) = spawn_server().await;
 { let db = unknown_factory(); }
 db.create_nonce();
}
async fn sibling() {
 let (_, db, _) = spawn_server().await;
 { let db = unknown_factory(); }
 { db.create_nonce(); }
}
async fn parameter_shadow(db: Unknown) { db.create_nonce(); }
async fn factory_shadow(spawn_server: Unknown) {
 let (_, db, _) = spawn_server().await;
 db.create_nonce();
}
async fn factory_local_shadow() {
 let spawn_server = unknown_factory();
 let (_, db, _) = spawn_server().await;
 db.create_nonce();
}
async fn unknown_tuple() {
 let (_, db, _) = unknown_factory().await;
 db.create_nonce();
}
async fn generic_guards(items: Vec<u8>, value: String) {
 items.len(); items.contains(&1);
 format!("{}", value.len());
}
async fn same_line_inside() { let (_, db, _) = spawn_server().await; { let db = unknown_factory(); db.create_nonce(); } }
async fn same_line_after() { let (_, db, _) = spawn_server().await; { let db = unknown_factory(); } db.create_nonce(); }
"#;
        let (files, edges) = review_edges(&[(path, source)], Language::Rust);
        let expected: std::collections::BTreeSet<_> =
            ["user", "after_inner", "sibling", "same_line_after"]
                .into_iter()
                .map(|name| uid(&files, path, name))
                .collect();
        assert_eq!(
            review_callers(&files, &edges, path, "create_nonce"),
            expected,
            "{files:#?} {edges:#?}"
        );
        assert_eq!(
            review_callers(&files, &edges, path, "register"),
            std::collections::BTreeSet::from([uid(&files, path, "user")]),
            "{edges:#?}"
        );
        for name in ["len", "contains"] {
            assert!(
                review_callers(&files, &edges, path, name).is_empty(),
                "{name}: {edges:#?}"
            );
        }
    }

    #[test]
    fn review2_later_const_is_referenced_from_deferred_function_body() {
        let path = "src/library.js";
        let (files, edges) = review_edges(
            &[(
                path,
                "function user() { return later(); }\nconst later = () => 1;\nuser();\nfunction shadow(later) { return later(); }\nfunction early() { local(); const local = () => 2; }\n",
            )],
            Language::JavaScript,
        );
        assert_eq!(
            review_callers(&files, &edges, path, "later"),
            std::collections::BTreeSet::from([uid(&files, path, "user")]),
            "{edges:#?}"
        );
        assert!(
            review_callers(&files, &edges, path, "local").is_empty(),
            "same-body TDZ: {edges:#?}"
        );
    }

    #[test]
    fn review2_constructor_field_evidence_refuses_reassignment_and_constructor_shadow() {
        for (members, expected) in [
            (
                "store;\n constructor() { this.store = new Store(); }\n run() { this.store.query(); }\n",
                true,
            ),
            (
                "store = new Store();\n replace() { this.store = unknownFactory(); }\n run() { this.store.query(); }\n",
                false,
            ),
            (
                "store;\n constructor(Store) { this.store = new Store(); }\n run() { this.store.query(); }\n",
                false,
            ),
            (
                "store;\n constructor() { this.store = unknownFactory(); }\n run() { this.store.query(); }\n",
                false,
            ),
        ] {
            let path = "src/library.js";
            let source =
                format!("class Store {{\n query() {{}}\n}}\nclass Controller {{\n{members}}}\n");
            let (files, edges) = review_edges(&[(path, &source)], Language::JavaScript);
            let expected = if expected {
                std::collections::BTreeSet::from([uid(&files, path, "run")])
            } else {
                std::collections::BTreeSet::new()
            };
            assert_eq!(
                review_callers(&files, &edges, path, "query"),
                expected,
                "{members}: {files:#?} {edges:#?}"
            );
        }
    }

    #[test]
    fn review2_scalar_factory_return_and_tuple_types_keep_import_origin() {
        let (files, edges) = review_edges(
            &[
                ("src/lib.rs", "mod storage; mod provider; mod user;\n"),
                (
                    "src/storage.rs",
                    "pub struct Storage;\nimpl Storage { pub fn create_nonce(&self) {} }\n",
                ),
                (
                    "other/storage.rs",
                    "pub struct Storage;\nimpl Storage { pub fn create_nonce(&self) {} }\n",
                ),
                (
                    "src/provider.rs",
                    "use crate::storage::Storage;\npub fn factory() -> Storage { todo!() }\n",
                ),
                (
                    "src/user.rs",
                    "use crate::provider::factory;\nfn user() { let db = factory(); db.create_nonce(); }\nfn shadow(factory: Unknown) { let db = factory(); db.create_nonce(); }\nfn inner() { let db = factory(); { let db = unknown(); db.create_nonce(); } }\nfn after() { let db = factory(); { let db = unknown(); } db.create_nonce(); }\n",
                ),
            ],
            Language::Rust,
        );
        assert_eq!(
            review_callers(&files, &edges, "src/storage.rs", "create_nonce"),
            ["user", "after"]
                .into_iter()
                .map(|name| uid(&files, "src/user.rs", name))
                .collect(),
            "{files:#?} {edges:#?}"
        );
        assert!(
            review_callers(&files, &edges, "other/storage.rs", "create_nonce").is_empty(),
            "wrong class origin: {edges:#?}"
        );
    }

    #[test]
    fn review2_field_constructor_alias_preserves_duplicate_class_origin() {
        let (files, edges) = review_edges(
            &[
                ("src/store.ts", "export class Store {\n query() {}\n}\n"),
                ("other/store.ts", "export class Store {\n query() {}\n}\n"),
                (
                    "src/user.ts",
                    "import { Store as Selected } from './store';\nclass Controller {\n store = new Selected();\n run() { this.store.query(); }\n}\nclass Unknown {\n store = unknownFactory();\n bad() { this.store.query(); }\n}\n",
                ),
            ],
            Language::TypeScript,
        );
        assert_eq!(
            review_callers(&files, &edges, "src/store.ts", "query"),
            std::collections::BTreeSet::from([uid(&files, "src/user.ts", "run")]),
            "{files:#?} {edges:#?}"
        );
        assert!(
            review_callers(&files, &edges, "other/store.ts", "query").is_empty(),
            "wrong class origin: {edges:#?}"
        );
    }

    #[test]
    fn review2_std_pointer_origin_refuses_custom_arc_receiver_types() {
        for (pointer, prefix, expected) in [
            ("Box<Storage>", "", true),
            ("std::boxed::Box<Storage>", "", true),
            (
                "Box<Storage>",
                "mod other { pub struct Box<T>(pub T); impl<T> Box<T> { pub fn create_nonce(&self) {} } }\nuse other::*;\n",
                false,
            ),
            ("std::sync::Arc<Storage>", "", true),
            ("Arc<Storage>", "use std::sync::Arc;\n", true),
            ("Shared<Storage>", "use std::sync::Arc as Shared;\n", true),
            (
                "Arc<Storage>",
                "struct Arc<T> { inner: T }\nimpl<T> Arc<T> { fn create_nonce(&self) {} }\n",
                false,
            ),
            (
                "other::Arc<Storage>",
                "mod other { pub struct Arc<T> { inner: T } }\n",
                false,
            ),
        ] {
            let path = "src/library.rs";
            let source = format!(
                "{prefix}struct Storage;\nimpl Storage {{ pub fn create_nonce(&self) {{}} }}\nfn user(db: {pointer}) {{ db.create_nonce(); }}\n"
            );
            let (files, edges) = review_edges(&[(path, &source)], Language::Rust);
            let storage = files[0]
                .1
                .iter()
                .find(|symbol| {
                    symbol.name == "create_nonce"
                        && symbol.parent_name.as_deref() == Some("Storage")
                })
                .unwrap();
            let target = symbol_uid("repo:test:abc", path, &storage.name, storage.start_line);
            let actual: std::collections::BTreeSet<_> = edges
                .iter()
                .filter(|edge| edge.edge_type == EdgeType::Calls && edge.target_uid == target)
                .map(|edge| edge.source_uid.clone())
                .collect();
            let expected = if expected {
                std::collections::BTreeSet::from([uid(&files, path, "user")])
            } else {
                std::collections::BTreeSet::new()
            };
            assert_eq!(actual, expected, "{pointer}: {files:#?} {edges:#?}");
        }
    }

    #[test]
    fn review2_rust_named_super_import_reaches_exact_parent_item() {
        let (files, edges) = review_edges(
            &[
                ("src/lib.rs", "mod child;\nfn helper() {}\n"),
                (
                    "src/child.rs",
                    "use super::helper;\nfn user() { helper(); }\n",
                ),
                ("other/lib.rs", "fn helper() {}\n"),
                ("src/outsider.rs", "fn outsider() { helper(); }\n"),
            ],
            Language::Rust,
        );
        assert_eq!(
            review_callers(&files, &edges, "src/lib.rs", "helper"),
            std::collections::BTreeSet::from([uid(&files, "src/child.rs", "user")]),
            "{edges:#?}"
        );
        assert!(review_callers(&files, &edges, "other/lib.rs", "helper").is_empty());
    }

    #[test]
    fn review5_rust_imported_type_qualified_functions_keep_exact_owner() {
        for (lib, provider, user, import) in [
            (
                "src/lib.rs",
                "src/storage.rs",
                "src/user.rs",
                "crate::storage::Storage",
            ),
            (
                "crates/storage-one/src/lib.rs",
                "crates/storage-one/src/storage.rs",
                "crates/client/src/lib.rs",
                "storage_one::storage::Storage",
            ),
        ] {
            let provider_source = "pub struct Storage;\nimpl Storage {\n pub fn open() -> Storage { Storage }\n pub fn method(&self) {}\n}\n";
            let user_source = format!(
                "use {import} as Selected;\nfn direct() {{ Selected::open(); }}\nfn parameter(db: &Selected) {{ db.method(); }}\nfn ufcs(db: &Selected) {{ Selected::method(db); }}\nfn shadow(Selected: Unknown) {{ Selected::open(); }}\nfn dot() {{ Selected.open(); }}\nfn returned() {{ let db = Selected::open(); db.method(); }}\n"
            );
            let (files, edges) = review_edges(
                &[
                    (lib, "pub mod storage;\nmod user;\n"),
                    (provider, provider_source),
                    (user, &user_source),
                    ("other/storage.rs", provider_source),
                ],
                Language::Rust,
            );
            assert_eq!(
                review_callers(&files, &edges, provider, "open"),
                ["direct", "returned"]
                    .into_iter()
                    .map(|name| uid(&files, user, name))
                    .collect(),
                "{import}: {files:#?} {edges:#?}"
            );
            assert_eq!(
                review_callers(&files, &edges, provider, "method"),
                ["parameter", "ufcs", "returned"]
                    .into_iter()
                    .map(|name| uid(&files, user, name))
                    .collect(),
                "{import}: {files:#?} {edges:#?}"
            );
            assert!(review_callers(&files, &edges, "other/storage.rs", "open").is_empty());
            assert!(review_callers(&files, &edges, "other/storage.rs", "method").is_empty());
        }
    }

    #[test]
    fn review5_rust_imported_type_refuses_function_local_owner_donation() {
        let provider = "src/storage.rs";
        let (files, edges) = review_edges(
            &[
                ("src/lib.rs", "mod storage; mod user;\n"),
                (
                    provider,
                    "pub struct Storage;\nfn hidden() { struct Storage; impl Storage { pub fn open() {} } }\n",
                ),
                (
                    "src/user.rs",
                    "use crate::storage::Storage;\nfn user() { Storage::open(); }\n",
                ),
            ],
            Language::Rust,
        );
        assert!(
            review_callers(&files, &edges, provider, "open").is_empty(),
            "{files:#?} {edges:#?}"
        );
    }

    #[test]
    fn review5_rust_imported_inline_and_reexported_type_paths_keep_terminal_owner() {
        let (files, edges) = review_edges(
            &[
                (
                    "crates/provider/src/lib.rs",
                    "pub mod process;\npub use process::testing::Runner as Exported;\n",
                ),
                (
                    "crates/provider/src/process.rs",
                    "pub mod testing {\n pub struct Runner;\n impl Runner { pub fn replying() {} }\n}\n",
                ),
                (
                    "crates/user/src/lib.rs",
                    "use provider::process::testing::Runner as Direct;\nuse provider::Exported as Alias;\nfn direct() { Direct::replying(); }\nfn alias() { Alias::replying(); }\nfn shadow(Direct: Unknown) { Direct::replying(); }\n",
                ),
                (
                    "other/process.rs",
                    "pub struct Runner;\nimpl Runner { pub fn replying() {} }\n",
                ),
            ],
            Language::Rust,
        );
        assert_eq!(
            review_callers(&files, &edges, "crates/provider/src/process.rs", "replying"),
            ["direct", "alias"]
                .into_iter()
                .map(|name| uid(&files, "crates/user/src/lib.rs", name))
                .collect(),
            "{files:#?} {edges:#?}"
        );
        assert!(review_callers(&files, &edges, "other/process.rs", "replying").is_empty());
    }

    #[test]
    fn review5_rust_inline_local_tuple_factory_keeps_parent_imported_type() {
        let user = "crates/user/src/lib.rs";
        let (files, edges) = review_edges(
            &[
                (
                    "crates/provider/src/lib.rs",
                    "pub struct Storage;\nimpl Storage { pub fn method(&self) {} }\n",
                ),
                (
                    user,
                    "use provider::Storage;\n#[cfg(test)] mod tests {\n use super::*;\n fn helper() -> ((), Storage) { todo!() }\n fn caller() { let (_, db) = helper(); db.method(); }\n fn unknown(helper: Unknown) { let (_, db) = helper(); db.method(); }\n fn shadow() { let (_, db) = helper(); { let db = unknown(); db.method(); } }\n}\n",
                ),
                (
                    "other/storage.rs",
                    "pub struct Storage;\nimpl Storage { pub fn method(&self) {} }\n",
                ),
            ],
            Language::Rust,
        );
        assert_eq!(
            review_callers(&files, &edges, "crates/provider/src/lib.rs", "method"),
            std::collections::BTreeSet::from([uid(&files, user, "caller")]),
            "{files:#?} {edges:#?}"
        );
        assert!(review_callers(&files, &edges, "other/storage.rs", "method").is_empty());
    }

    #[test]
    fn review5_rust_qualified_inner_named_import_shadows_outer_owner() {
        let (files, edges) = review_edges(
            &[
                ("src/lib.rs", "mod outer; mod inner; mod user;\n"),
                (
                    "src/outer.rs",
                    "pub struct Storage;\nimpl Storage { pub fn open() {} }\n",
                ),
                (
                    "src/inner.rs",
                    "pub struct Storage;\nimpl Storage { pub fn open() {} }\n",
                ),
                (
                    "src/user.rs",
                    "use crate::outer::Storage;\nfn outer() { Storage::open(); }\nmod tests { use crate::inner::Storage;\n fn inner() { Storage::open(); }\n}\n",
                ),
            ],
            Language::Rust,
        );
        assert_eq!(
            review_callers(&files, &edges, "src/outer.rs", "open"),
            std::collections::BTreeSet::from([uid(&files, "src/user.rs", "outer")]),
            "{files:#?} {edges:#?}"
        );
        assert_eq!(
            review_callers(&files, &edges, "src/inner.rs", "open"),
            std::collections::BTreeSet::from([uid(&files, "src/user.rs", "inner")]),
            "{files:#?} {edges:#?}"
        );
    }

    #[test]
    fn review5_rust_tuple_return_origin_precedes_caller_local_type_shadow() {
        let provider = "crates/provider/src/lib.rs";
        let user = "crates/user/src/lib.rs";
        let (files, edges) = review_edges(
            &[
                (
                    provider,
                    "pub struct Storage;\nimpl Storage { pub fn method(&self) {} }\n",
                ),
                (
                    user,
                    "use provider::Storage;\nmod tests { use super::*;\n fn helper() -> ((), Storage) { todo!() }\n fn caller() {\n  struct Storage;\n  impl Storage { fn method(&self) {} }\n  let (_, db) = helper();\n  db.method();\n }\n}\n",
                ),
            ],
            Language::Rust,
        );
        assert_eq!(
            review_callers(&files, &edges, provider, "method"),
            std::collections::BTreeSet::from([uid(&files, user, "caller")]),
            "{files:#?} {edges:#?}"
        );
        assert!(
            review_callers(&files, &edges, user, "method").is_empty(),
            "caller-local type cannot donate the factory's return: {edges:#?}"
        );
    }

    #[test]
    fn review5_rust_parent_glob_respects_inner_named_alias_precedence() {
        let user = "crates/user/src/lib.rs";
        let provider_source = "pub struct Storage;\nimpl Storage { pub fn method(&self) {} }\n";
        let (files, edges) = review_edges(
            &[
                ("crates/outer/src/lib.rs", provider_source),
                ("crates/inner/src/lib.rs", provider_source),
                (
                    user,
                    "use outer::Storage;\nmod tests { use super::*; use inner::Storage as Storage;\n fn helper() -> ((), Storage) { todo!() }\n fn caller() { let (_, db) = helper(); db.method(); }\n}\n",
                ),
            ],
            Language::Rust,
        );
        assert_eq!(
            review_callers(&files, &edges, "crates/inner/src/lib.rs", "method"),
            std::collections::BTreeSet::from([uid(&files, user, "caller")]),
            "{files:#?} {edges:#?}"
        );
        assert!(
            review_callers(&files, &edges, "crates/outer/src/lib.rs", "method").is_empty(),
            "{edges:#?}"
        );
    }

    #[test]
    fn review5_rust_parent_glob_respects_module_local_type_override() {
        let user = "crates/user/src/lib.rs";
        let (files, edges) = review_edges(
            &[
                (
                    "crates/provider/src/lib.rs",
                    "pub struct Storage;\nimpl Storage { pub fn method(&self) {} }\n",
                ),
                (
                    user,
                    "use provider::Storage;\nmod tests { use super::*;\n struct Storage;\n impl Storage { fn method(&self) {} }\n fn helper() -> ((), Storage) { todo!() }\n fn caller() { let (_, db) = helper(); db.method(); }\n}\n",
                ),
            ],
            Language::Rust,
        );
        assert_eq!(
            review_callers(&files, &edges, user, "method"),
            std::collections::BTreeSet::from([uid(&files, user, "caller")]),
            "{files:#?} {edges:#?}"
        );
        assert!(
            review_callers(&files, &edges, "crates/provider/src/lib.rs", "method").is_empty(),
            "{edges:#?}"
        );
    }

    #[test]
    fn review5_rust_nearer_unsupported_tuple_factory_defeats_outer_evidence() {
        for nearest in [
            "type OtherTuple = (Unknown,); fn helper() -> OtherTuple { todo!() }",
            "fn helper<T>(_: T) -> (Unknown,) { todo!() }",
        ] {
            let call = if nearest.contains("<T>") {
                "helper(())"
            } else {
                "helper()"
            };
            let path = "src/lib.rs";
            let source = format!(
                "struct Storage;\nimpl Storage {{ fn method(&self) {{}} }}\nstruct Unknown;\nfn helper() -> (Storage,) {{ todo!() }}\nfn parent() {{ let (db,) = helper(); db.method(); }}\nfn nested() {{\n {nearest}\n let (db,) = {call};\n db.method();\n}}\n"
            );
            let (files, edges) = review_edges(&[(path, &source)], Language::Rust);
            assert_eq!(
                review_callers(&files, &edges, path, "method"),
                std::collections::BTreeSet::from([uid(&files, path, "parent")]),
                "nearest unsupported return must not donate an outer type: {nearest}: {files:#?} {edges:#?}"
            );
        }
    }

    #[test]
    fn review6_rust_same_file_sibling_inline_type_constructor_keeps_value_method() {
        for import in [
            "use super::testing::TempTree;",
            "use super::testing::TempTree as Selected;",
        ] {
            let selected = if import.contains(" as ") {
                "Selected"
            } else {
                "TempTree"
            };
            let path = "src/lib.rs";
            let source = format!(
                "pub mod testing {{\n pub struct TempTree;\n impl TempTree {{\n  pub fn new() -> Self {{ Self }}\n  pub fn layout(&self) {{}}\n }}\n}}\nmod tests {{\n {import}\n fn caller() {{ let tree = {selected}::new(); tree.layout(); }}\n fn shadow(value: Unknown) {{ let tree = value.new(); tree.layout(); }}\n fn unknown() {{ let tree = unknown(); tree.layout(); }}\n}}\n"
            );
            let (files, edges) = review_edges(&[(path, &source)], Language::Rust);
            let expected = std::collections::BTreeSet::from([uid(&files, path, "caller")]);
            assert_eq!(
                review_callers(&files, &edges, path, "new"),
                expected,
                "{import}: {files:#?} {edges:#?}"
            );
            assert_eq!(
                review_callers(&files, &edges, path, "layout"),
                expected,
                "{import}: {files:#?} {edges:#?}"
            );
        }
    }

    #[test]
    fn review6_rust_same_file_inline_import_refuses_same_name_wrong_module() {
        let path = "src/lib.rs";
        let source = "mod wrong { pub struct TempTree; impl TempTree { pub fn new() -> Self { Self } pub fn layout(&self) {} } }\nmod testing { pub struct TempTree; impl TempTree { pub fn new() -> Self { Self } pub fn layout(&self) {} } }\nmod tests { use super::testing::TempTree as Selected; fn caller() { let tree = Selected::new(); tree.layout(); } }\n";
        let (files, edges) = review_edges(&[(path, source)], Language::Rust);
        for name in ["new", "layout"] {
            let candidates: Vec<_> = files[0]
                .1
                .iter()
                .filter(|symbol| symbol.name == name)
                .collect();
            assert_eq!(candidates.len(), 2, "{files:#?}");
            for candidate in candidates {
                let actual: std::collections::BTreeSet<_> = edges
                    .iter()
                    .filter(|edge| {
                        edge.edge_type == EdgeType::Calls
                            && edge.target_uid
                                == symbol_uid("repo:test:abc", path, name, candidate.start_line)
                    })
                    .map(|edge| edge.source_uid.clone())
                    .collect();
                let expected = if candidate.start_line == 2 {
                    std::collections::BTreeSet::from([uid(&files, path, "caller")])
                } else {
                    std::collections::BTreeSet::new()
                };
                assert_eq!(actual, expected, "{name}: {candidate:#?} {edges:#?}");
            }
        }
    }

    #[test]
    fn review6_rust_inline_type_route_boundaries_and_block_lifetime() {
        for (middle, selected) in [
            (
                "use self::testing::TempTree as Selected;\nfn caller() { let tree = Selected::new(); tree.layout(); }\n",
                "caller",
            ),
            (
                "mod tests { mod nested { use super::super::testing::TempTree as Selected;\nfn caller() { let tree = Selected::new(); tree.layout(); }\n}}\n",
                "caller",
            ),
            (
                "fn caller() { { use self::testing::TempTree as Selected; let tree = Selected::new(); tree.layout(); } }\nfn outside() { let tree = Selected::new(); tree.layout(); }\n",
                "caller",
            ),
        ] {
            let path = "src/lib.rs";
            let source = format!(
                "pub mod testing {{\n pub struct TempTree;\n impl TempTree {{ pub fn new() -> Self {{ Self }} pub fn layout(&self) {{}} }}\n}}\n{middle}"
            );
            let (files, edges) = review_edges(&[(path, &source)], Language::Rust);
            for name in ["new", "layout"] {
                assert_eq!(
                    review_callers(&files, &edges, path, name),
                    std::collections::BTreeSet::from([uid(&files, path, selected)]),
                    "{middle}: {files:#?} {edges:#?}"
                );
            }
        }
    }

    #[test]
    fn review6_rust_inline_type_route_overrides_only_its_physical_file_guess() {
        let path = "src/lib.rs";
        let source = "pub mod testing {\n pub struct TempTree;\n impl TempTree { pub fn new() -> Self { Self } pub fn layout(&self) {} }\n}\nmod tests { use super::testing::TempTree as Selected; fn caller() { let tree = Selected::new(); tree.layout(); } }\n";
        let decoy = "src/testing.rs";
        let (files, edges) = review_edges(
            &[
                (path, source),
                (
                    decoy,
                    "pub struct TempTree;\nimpl TempTree { pub fn new() -> Self { Self } pub fn layout(&self) {} }\n",
                ),
            ],
            Language::Rust,
        );
        for name in ["new", "layout"] {
            assert_eq!(
                review_callers(&files, &edges, path, name),
                std::collections::BTreeSet::from([uid(&files, path, "caller")]),
                "{files:#?} {edges:#?}"
            );
            assert!(
                review_callers(&files, &edges, decoy, name).is_empty(),
                "unused physical file is not the declared inline module: {edges:#?}"
            );
        }
    }

    #[test]
    fn review6_rust_inline_type_route_refuses_parent_escape_and_external_guess() {
        for import in [
            "use super::super::testing::TempTree as Selected;",
            "use external::testing::TempTree as Selected;",
        ] {
            let path = "src/lib.rs";
            let source = format!(
                "pub mod testing {{\n pub struct TempTree;\n impl TempTree {{ pub fn new() -> Self {{ Self }} pub fn layout(&self) {{}} }}\n}}\nmod tests {{ {import} fn wrong() {{ let tree = Selected::new(); tree.layout(); }} }}\n"
            );
            let (files, edges) = review_edges(&[(path, &source)], Language::Rust);
            for name in ["new", "layout"] {
                assert!(
                    review_callers(&files, &edges, path, name).is_empty(),
                    "{import}: {files:#?} {edges:#?}"
                );
            }
        }
    }

    #[test]
    fn review7_rust_direct_private_parent_type_preserves_constructor_and_ufcs() {
        for (import, selected) in [
            ("use super::Local;", "Local"),
            ("use super::Local as Selected;", "Selected"),
        ] {
            let path = "src/lib.rs";
            let source = format!(
                "struct Local;\nimpl Local {{\n fn new() -> Self {{ Self }}\n fn run(&self) {{}}\n}}\nmod tests {{ {import}\n fn caller() {{ let x = {selected}::new(); x.run(); }}\n fn ufcs(x: &{selected}) {{ {selected}::run(x); }}\n}}\n"
            );
            let (files, edges) = review_edges(&[(path, &source)], Language::Rust);
            for method in ["new", "run"] {
                let expected = if method == "run" {
                    std::collections::BTreeSet::from([
                        uid(&files, path, "caller"),
                        uid(&files, path, "ufcs"),
                    ])
                } else {
                    std::collections::BTreeSet::from([uid(&files, path, "caller")])
                };
                assert_eq!(
                    review_callers(&files, &edges, path, method),
                    expected,
                    "{import}: {files:#?} {edges:#?}"
                );
            }
        }
    }

    #[test]
    fn review7_rust_private_parent_type_keeps_exact_root_owner_and_file() {
        let (files, edges) = review_edges(
            &[
                (
                    "src/lib.rs",
                    "mod child;\nstruct Local;\nimpl Local { fn new() -> Self { Self } fn run(&self) {} }\nfn hidden() { struct Local; impl Local { fn new() -> Self { Self } fn run(&self) {} } }\n",
                ),
                (
                    "src/child.rs",
                    "use super::Local as Selected;\nfn caller() { let x = Selected::new(); x.run(); }\n",
                ),
                (
                    "other/lib.rs",
                    "struct Local;\nimpl Local { fn new() -> Self { Self } fn run(&self) {} }\n",
                ),
            ],
            Language::Rust,
        );
        for name in ["new", "run"] {
            let root = files[0]
                .1
                .iter()
                .find(|symbol| symbol.name == name)
                .unwrap();
            let actual: std::collections::BTreeSet<_> = edges
                .iter()
                .filter(|edge| {
                    edge.edge_type == EdgeType::Calls
                        && edge.target_uid
                            == symbol_uid("repo:test:abc", "src/lib.rs", name, root.start_line)
                })
                .map(|edge| edge.source_uid.clone())
                .collect();
            assert_eq!(
                actual,
                std::collections::BTreeSet::from([uid(&files, "src/child.rs", "caller")]),
                "{files:#?} {edges:#?}"
            );
            assert!(review_callers(&files, &edges, "other/lib.rs", name).is_empty());
            for hidden in files[0]
                .1
                .iter()
                .filter(|symbol| symbol.name == name && symbol.start_line != root.start_line)
            {
                assert!(
                    !edges.iter().any(|edge| edge.edge_type == EdgeType::Calls
                        && edge.target_uid
                            == symbol_uid("repo:test:abc", "src/lib.rs", name, hidden.start_line)),
                    "hidden function-local owner: {edges:#?}"
                );
            }
        }
    }

    #[test]
    fn review7_rust_private_type_cannot_be_forwarded_through_public_barrel() {
        let (files, edges) = review_edges(
            &[
                (
                    "crates/provider/src/lib.rs",
                    "mod hidden;\npub use hidden::Local as Exported;\n",
                ),
                (
                    "crates/provider/src/hidden.rs",
                    "struct Local;\nimpl Local { pub fn new() -> Self { Self } pub fn run(&self) {} }\n",
                ),
                (
                    "crates/user/src/lib.rs",
                    "use provider::Exported as Selected;\nfn caller() { let x = Selected::new(); x.run(); }\n",
                ),
            ],
            Language::Rust,
        );
        for name in ["new", "run"] {
            assert!(
                review_callers(&files, &edges, "crates/provider/src/hidden.rs", name).is_empty(),
                "private terminal is not a public reexport: {edges:#?}"
            );
        }
    }

    #[test]
    fn review7_rust_private_sibling_inline_type_is_not_a_root_parent_import() {
        let path = "src/lib.rs";
        let source = "mod hidden {\n struct Local;\n impl Local { pub fn new() -> Self { Self } pub fn run(&self) {} }\n}\nmod tests { use super::hidden::Local as Selected; fn wrong() { let x = Selected::new(); x.run(); } }\n";
        let (files, edges) = review_edges(&[(path, source)], Language::Rust);
        for name in ["new", "run"] {
            assert!(
                review_callers(&files, &edges, path, name).is_empty(),
                "private inline terminal is not a root-private parent type: {files:#?} {edges:#?}"
            );
        }
    }

    #[test]
    fn review7_rust_public_root_type_private_methods_preserve_constructor_and_ufcs() {
        for (import, selected) in [
            ("use super::Local;", "Local"),
            ("use super::Local as Selected;", "Selected"),
        ] {
            let path = "src/lib.rs";
            let source = format!(
                "pub struct Local;\nimpl Local {{\n fn new() -> Self {{ Self }}\n fn run(&self) {{}}\n}}\nmod tests {{ {import}\n fn caller() {{ let x = {selected}::new(); x.run(); }}\n fn ufcs(x: &{selected}) {{ {selected}::run(x); }}\n}}\n"
            );
            let (files, edges) = review_edges(&[(path, &source)], Language::Rust);
            assert_eq!(
                review_callers(&files, &edges, path, "new"),
                std::collections::BTreeSet::from([uid(&files, path, "caller")]),
                "{import}: {files:#?} {edges:#?}"
            );
            assert_eq!(
                review_callers(&files, &edges, path, "run"),
                std::collections::BTreeSet::from([
                    uid(&files, path, "caller"),
                    uid(&files, path, "ufcs")
                ]),
                "{import}: {files:#?} {edges:#?}"
            );
        }
    }

    #[test]
    fn review7_rust_public_inline_type_private_methods_remain_sibling_private() {
        let path = "src/lib.rs";
        let source = "pub mod hidden {\n pub struct Local;\n impl Local { fn new() -> Self { Self } fn run(&self) {} }\n}\nmod tests {\n use super::hidden::Local as Selected;\n fn caller() { let x = Selected::new(); x.run(); }\n fn ufcs(x: &Selected) { Selected::run(x); }\n}\n";
        let (files, edges) = review_edges(&[(path, source)], Language::Rust);
        assert!(
            review_callers(&files, &edges, path, "new").is_empty(),
            "private sibling associated method: {edges:#?}"
        );
        // `x.run` already uses the existing same-file typed member admission;
        // this regression fix must not additionally admit sibling UFCS.
        let callers = review_callers(&files, &edges, path, "run");
        assert!(
            !callers.contains(&uid(&files, path, "ufcs")),
            "private sibling UFCS: {files:#?} {edges:#?}"
        );
    }

    #[test]
    fn review2_python_imported_class_static_method_keeps_shadow_guard() {
        let (files, edges) = review_edges(
            &[
                (
                    "pkg/service.py",
                    "class Service:\n    @staticmethod\n    def run():\n        return 1\n",
                ),
                (
                    "pkg/user.py",
                    "from .service import Service\ndef user():\n    return Service.run()\ndef shadow(Service):\n    return Service.run()\n",
                ),
                (
                    "other/service.py",
                    "class Service:\n    @staticmethod\n    def run():\n        return 2\n",
                ),
            ],
            Language::Python,
        );
        assert_eq!(
            review_callers(&files, &edges, "pkg/service.py", "run"),
            std::collections::BTreeSet::from([uid(&files, "pkg/user.py", "user")]),
            "{edges:#?}"
        );
        assert!(review_callers(&files, &edges, "other/service.py", "run").is_empty());
    }

    #[test]
    fn review2_js_constructor_control_uses_real_scoped_environment() {
        let (files, edges) = review_edges(
            &[(
                "src/library.js",
                "class Service {\n run() {}\n}\nfunction user() {\n const s = new Service();\n s.run();\n}\nfunction shadow(s) { s.run(); }\n",
            )],
            Language::JavaScript,
        );
        assert_eq!(
            review_callers(&files, &edges, "src/library.js", "run"),
            std::collections::BTreeSet::from([uid(&files, "src/library.js", "user")]),
            "{edges:#?}"
        );
    }

    #[test]
    fn review_commonjs_export_calls_local_private_helper() {
        let (files, edges) = review_edges(
            &[
                ("index.js", "const h = require('./helpers');\nh.listen();\n"),
                (
                    "helpers.js",
                    "module.exports.listen = function listen() {\n return context();\n};\nfunction context() {}\nfunction unused() {}\n",
                ),
            ],
            Language::JavaScript,
        );
        assert_eq!(
            review_callers(&files, &edges, "helpers.js", "context"),
            std::collections::BTreeSet::from([uid(&files, "helpers.js", "listen")]),
            "files={files:#?} edges={edges:#?}"
        );
        assert!(review_callers(&files, &edges, "helpers.js", "unused").is_empty());
        assert!(
            files[1]
                .1
                .iter()
                .any(|symbol| symbol.name == "listen" && symbol.is_entry_point),
            "exported executable must root its private helper: {files:#?}"
        );
    }

    #[test]
    fn review_parsed_typed_receiver_reaches_inherited_method() {
        let (files, edges) = review_edges(
            &[(
                "src/main.ts",
                "class Base {\n run() { return 1; }\n}\nclass Child extends Base {}\nfunction user() {\n const child = new Child();\n return child.run();\n}\nfunction shadow(child: unknown) { return child.run(); }\n",
            )],
            Language::TypeScript,
        );
        assert_eq!(
            review_callers(&files, &edges, "src/main.ts", "run"),
            std::collections::BTreeSet::from([uid(&files, "src/main.ts", "user")]),
            "files={files:#?} edges={edges:#?}"
        );
    }

    #[test]
    fn review_go_lowercase_sibling_is_visible_only_within_package() {
        let (files, edges) = review_edges(
            &[
                (
                    "pkg/helper.go",
                    "package sample\nfunc helper() int { return 1 }\n",
                ),
                (
                    "pkg/user.go",
                    "package sample\nfunc user() int {\n return helper()\n}\n",
                ),
                (
                    "other/user.go",
                    "package other\nfunc outsider() int {\n return helper()\n}\n",
                ),
                (
                    "pkg/external_test.go",
                    "package sample_test\nfunc external() int {\n return helper()\n}\n",
                ),
            ],
            Language::Go,
        );
        assert_eq!(
            review_callers(&files, &edges, "pkg/helper.go", "helper"),
            std::collections::BTreeSet::from([uid(&files, "pkg/user.go", "user")]),
            "{edges:#?}"
        );
    }

    #[test]
    fn review_rust_child_glob_can_call_parent_private_helper() {
        let (files, edges) = review_edges(
            &[
                ("src/lib.rs", "mod tests;\nfn helper() -> usize { 1 }\n"),
                (
                    "src/tests.rs",
                    "use super::*;\nfn user() {\n helper();\n}\n",
                ),
                ("other/outsider.rs", "fn outsider() {\n helper();\n}\n"),
            ],
            Language::Rust,
        );
        assert_eq!(
            review_callers(&files, &edges, "src/lib.rs", "helper"),
            std::collections::BTreeSet::from([uid(&files, "src/tests.rs", "user")]),
            "{edges:#?}"
        );
    }

    #[test]
    fn review_python_explicit_private_import_and_shadow_counterweight() {
        let (files, edges) = review_edges(
            &[
                ("pkg/helper.py", "def _helper():\n    return 1\n"),
                (
                    "pkg/user.py",
                    "from .helper import _helper\ndef user():\n    return _helper()\ndef shadow(_helper):\n    return _helper()\n",
                ),
                ("other/user.py", "def outsider():\n    return _helper()\n"),
            ],
            Language::Python,
        );
        assert_eq!(
            review_callers(&files, &edges, "pkg/helper.py", "_helper"),
            std::collections::BTreeSet::from([uid(&files, "pkg/user.py", "user")]),
            "{edges:#?}"
        );
    }

    #[test]
    fn review_local_receiver_type_precedes_unknown_receiver_refusal() {
        for (path, language, source, method) in [
            (
                "src/pool.rs",
                Language::Rust,
                "struct Pool;\nimpl Pool {\n fn len(&self) -> usize { 1 }\n}\nfn typed(pool: &Pool) -> usize {\n pool.len()\n}\nfn unrelated(items: Vec<u8>) -> usize {\n items.len()\n}\n",
                "len",
            ),
            (
                "src/service.ts",
                Language::TypeScript,
                "class Service {\n run() { return 1; }\n}\nfunction typed() {\n const svc = new Service();\n return svc.run();\n}\nfunction unrelated(svc: unknown) {\n return svc.run();\n}\n",
                "run",
            ),
        ] {
            let (files, edges) = review_edges(&[(path, source)], language);
            assert_eq!(
                review_callers(&files, &edges, path, method),
                std::collections::BTreeSet::from([uid(&files, path, "typed")]),
                "{path}: {edges:#?}"
            );
        }
    }

    #[test]
    fn review_named_and_default_imported_static_receivers() {
        let (files, edges) = review_edges(
            &[
                (
                    "src/logger.js",
                    "export class Logger {\n static write() { return 1; }\n}\n",
                ),
                (
                    "src/db.js",
                    "export default class Database {\n static query() { return 2; }\n}\n",
                ),
                (
                    "src/user.js",
                    "import { Logger } from './logger.js';\nimport db from './db.js';\nfunction user() {\n Logger.write();\n db.query();\n}\nfunction shadow(Logger, db) {\n Logger.write();\n db.query();\n}\n",
                ),
            ],
            Language::JavaScript,
        );
        for (path, method) in [("src/logger.js", "write"), ("src/db.js", "query")] {
            assert_eq!(
                review_callers(&files, &edges, path, method),
                std::collections::BTreeSet::from([uid(&files, "src/user.js", "user")]),
                "{edges:#?}"
            );
        }
    }

    #[test]
    fn review_unrelated_import_preserves_same_file_calls_only() {
        let (files, edges) = review_edges(
            &[
                (
                    "src/other.js",
                    "export function spare() { return 0; }\nexport function helper() { return 0; }\n",
                ),
                (
                    "src/user.js",
                    "import { spare } from './other.js';\nfunction helper() { return 1; }\nfunction user() {\n return helper();\n}\nfunction unrelated(items) {\n return items.helper();\n}\n",
                ),
            ],
            Language::JavaScript,
        );
        assert_eq!(
            review_callers(&files, &edges, "src/user.js", "helper"),
            std::collections::BTreeSet::from([uid(&files, "src/user.js", "user")]),
            "{edges:#?}"
        );
        assert!(
            review_callers(&files, &edges, "src/other.js", "helper").is_empty(),
            "{edges:#?}"
        );
    }

    #[test]
    fn review_star_barrels_follow_named_exports_without_default_or_private_leaks() {
        let (files, edges) = review_edges(
            &[
                (
                    "src/helper.ts",
                    "export function helper() { return 1; }\nfunction hidden() { return 2; }\nexport default function fallback() { return 3; }\n",
                ),
                ("src/middle.ts", "export * from './helper';\n"),
                ("src/index.ts", "export * from './middle';\n"),
                (
                    "src/user.ts",
                    "import { helper, hidden } from './index';\nimport fallback from './index';\nfunction user() {\n helper();\n hidden();\n fallback();\n}\n",
                ),
            ],
            Language::TypeScript,
        );
        assert_eq!(
            review_callers(&files, &edges, "src/helper.ts", "helper"),
            std::collections::BTreeSet::from([uid(&files, "src/user.ts", "user")]),
            "{edges:#?}"
        );
        for name in ["hidden", "fallback"] {
            assert!(
                review_callers(&files, &edges, "src/helper.ts", name).is_empty(),
                "{edges:#?}"
            );
        }
    }

    #[test]
    fn review_conflicting_star_exports_do_not_select_arbitrary_target() {
        let (files, edges) = review_edges(
            &[
                ("src/a.ts", "export function helper() { return 1; }\n"),
                ("src/b.ts", "export function helper() { return 2; }\n"),
                (
                    "src/index.ts",
                    "export * from './a';\nexport * from './b';\n",
                ),
                (
                    "src/user.ts",
                    "import { helper } from './index';\nfunction user() {\n helper();\n}\n",
                ),
            ],
            Language::TypeScript,
        );
        for path in ["src/a.ts", "src/b.ts"] {
            assert!(
                review_callers(&files, &edges, path, "helper").is_empty(),
                "{edges:#?}"
            );
        }
    }

    #[test]
    fn review_typescript_overload_declarations_select_one_implementation() {
        let (files, edges) = review_edges(
            &[
                (
                    "src/helper.ts",
                    "export function pick(value: string): string;\nexport function pick(value: number): number;\nexport function pick(value: string | number): string | number {\n return value;\n}\n",
                ),
                (
                    "src/user.ts",
                    "import { pick } from './helper';\nfunction user() {\n return pick('text');\n}\nfunction shadow(pick: () => string) {\n return pick();\n}\n",
                ),
            ],
            Language::TypeScript,
        );
        let implementation = files[0]
            .1
            .iter()
            .find(|symbol| symbol.name == "pick" && symbol.end_line > symbol.start_line)
            .expect("parsed overload implementation");
        let target = symbol_uid(
            "repo:test:abc",
            "src/helper.ts",
            "pick",
            implementation.start_line,
        );
        let actual: std::collections::BTreeSet<_> = edges
            .iter()
            .filter(|edge| edge.edge_type == EdgeType::Calls && edge.target_uid == target)
            .map(|edge| edge.source_uid.clone())
            .collect();
        assert_eq!(
            actual,
            std::collections::BTreeSet::from([uid(&files, "src/user.ts", "user")]),
            "{edges:#?}"
        );
    }

    #[test]
    fn review_default_parameter_initializers_are_uses_in_typescript_and_python() {
        for (language, helper_path, user_path, helper, source) in [
            (
                Language::TypeScript,
                "src/helper.ts",
                "src/user.ts",
                "export function helper() { return 1; }\n",
                "import { helper } from './helper';\nfunction consume(fallback: number = helper()) {\n return fallback;\n}\nfunction shadow(helper: () => number) {\n return helper();\n}\n",
            ),
            (
                Language::Python,
                "pkg/helper.py",
                "pkg/user.py",
                "def helper():\n    return 1\n",
                "from .helper import helper\ndef consume(fallback=helper()):\n    return fallback\ndef shadow(helper):\n    return helper()\n",
            ),
        ] {
            let (files, edges) =
                review_edges(&[(helper_path, helper), (user_path, source)], language);
            assert_eq!(
                review_callers(&files, &edges, helper_path, "helper"),
                std::collections::BTreeSet::from([uid(&files, user_path, "consume")]),
                "{user_path}: {edges:#?}"
            );
        }
    }
    #[test]
    fn review_receiver_shadow_scopes_restore_outer_types() {
        let path = "src/scopes.ts";
        let source = r#"class Service {
 run() { return 1; }
}
class Other {
 run() { return 2; }
}
function before() {
 const svc = new Service();
 return svc.run();
}
function inside() {
 const svc = new Service();
 { const svc = unknownFactory();
   return svc.run(); }
}
function after() {
 const svc = new Service();
 { const svc = unknownFactory(); }
 return svc.run();
}
function sibling() {
 const svc = new Service();
 { const svc = unknownFactory(); }
 { return svc.run(); }
}
function knownInner() {
 const svc = new Service();
 { const svc = new Other();
   return svc.run(); }
}
function afterKnownInner() {
 const svc = new Service();
 { const svc = new Other(); }
 return svc.run();
}
function sameLineInside() { const svc = new Service(); { const svc = unknownFactory(); return svc.run(); } }
function sameLineAfter() { const svc = new Service(); { const svc = unknownFactory(); } return svc.run(); }
function sameLineKnownInner() { const svc = new Service(); { const svc = new Other(); return svc.run(); } }
function parameterShadow(svc: unknown) { return svc.run(); }
function temporalDeadZone() {
 const svc = new Service();
 { svc.run(); const svc = unknownFactory(); }
}
function varBeforeInitialization() { svc.run(); var svc = new Service(); }
function varAfterInitialization() { var svc = new Service(); return svc.run(); }
"#;
        let (files, edges) = review_edges(&[(path, source)], Language::TypeScript);
        let target = |parent: &str| {
            let symbol = files[0]
                .1
                .iter()
                .find(|symbol| {
                    symbol.name == "run" && symbol.parent_name.as_deref() == Some(parent)
                })
                .unwrap();
            symbol_uid("repo:test:abc", path, "run", symbol.start_line)
        };
        let callers = |target: String| {
            edges
                .iter()
                .filter(|edge| edge.edge_type == EdgeType::Calls && edge.target_uid == target)
                .map(|edge| edge.source_uid.clone())
                .collect::<std::collections::BTreeSet<_>>()
        };
        assert_eq!(
            callers(target("Service")),
            [
                "before",
                "after",
                "sibling",
                "afterKnownInner",
                "sameLineAfter",
                "varAfterInitialization"
            ]
            .into_iter()
            .map(|name| uid(&files, path, name))
            .collect(),
            "{files:#?} {edges:#?}"
        );
        assert_eq!(
            callers(target("Other")),
            ["knownInner", "sameLineKnownInner"]
                .into_iter()
                .map(|name| uid(&files, path, name))
                .collect(),
            "{files:#?} {edges:#?}"
        );
    }

    /// Targets of the CALLS edges whose source is `caller` in `path`.
    fn call_targets(
        files: &ReviewParsedFiles,
        edges: &[ResolvedEdge],
        path: &str,
        caller: &str,
    ) -> Vec<String> {
        let source = uid(files, path, caller);
        let mut targets: Vec<String> = edges
            .iter()
            .filter(|edge| edge.source_uid == source && edge.edge_type == EdgeType::Calls)
            .map(|edge| edge.target_uid.clone())
            .collect();
        targets.sort();
        targets.dedup();
        targets
    }

    /// `uid` for the symbol named `name` declared on `line` (1-based) in `path`.
    fn uid_at(files: &ReviewParsedFiles, path: &str, name: &str, line: u32) -> String {
        let symbol = files
            .iter()
            .find(|(file, _, _)| file == path)
            .unwrap()
            .1
            .iter()
            .find(|symbol| symbol.name == name && symbol.start_line == line)
            .unwrap_or_else(|| panic!("no {name} declared on line {line} of {path}"));
        symbol_uid("repo:test:abc", path, name, symbol.start_line)
    }

    /// nw-113 (block scope). The block-local `sendPacket` exists only inside
    /// its block; after the block the name is the import again (a JS runtime
    /// control returns the imported provider's 7).
    #[test]
    fn a_block_local_callable_does_not_shadow_an_import_outside_its_block() {
        let lib = (
            "src/lib.ts",
            "export function sendPacket(): number { return 7; }\n",
        );
        let lexical = (
            "src/lexical.ts",
            r#"import { sendPacket } from "./lib";
export function namedCaller() {
  return sendPacket();
}
export function lexicalCaller() {
  {
    const sendPacket = () => 42;
    sendPacket();
  }
  return sendPacket();
}
export function innerOnly() {
  {
    const sendPacket = () => 1;
    return sendPacket();
  }
}
"#,
        );
        let (files, edges) = review_edges(&[lib, lexical], Language::TypeScript);
        let imported = uid(&files, "src/lib.ts", "sendPacket");
        let block_local = uid_at(&files, "src/lexical.ts", "sendPacket", 7);
        let inner_only_local = uid_at(&files, "src/lexical.ts", "sendPacket", 14);

        // Control: plain import resolution works in this fixture.
        assert_eq!(
            call_targets(&files, &edges, "src/lexical.ts", "namedCaller"),
            vec![imported.clone()]
        );
        // Inside the block: the block-local. After it: the import.
        let mut expected = vec![block_local, imported.clone()];
        expected.sort();
        assert_eq!(
            call_targets(&files, &edges, "src/lexical.ts", "lexicalCaller"),
            expected
        );
        // Counterweight: a call that never leaves the block stays local.
        assert_eq!(
            call_targets(&files, &edges, "src/lexical.ts", "innerOnly"),
            vec![inner_only_local]
        );
    }

    /// nw-113 (watcher parity). The watcher and the CLI incremental index
    /// resolve a changed file ALONE before re-resolving cross-file edges, so
    /// the import's source file is absent and its binding is unresolved. The
    /// block-local declaration must still shadow that binding inside its
    /// block, exactly as it does when the imported file is present.
    #[test]
    fn a_block_local_callable_shadows_an_import_whose_source_is_absent() {
        let lexical = (
            "src/lexical.ts",
            r#"import { sendPacket } from "./lib";
export function lexicalCaller() {
  {
    const sendPacket = () => 42;
    sendPacket();
  }
  return sendPacket();
}
"#,
        );
        let (files, edges) = review_edges(&[lexical], Language::TypeScript);
        let block_local = uid_at(&files, "src/lexical.ts", "sendPacket", 4);
        let mut expected = vec![block_local, "unresolved:sendPacket".to_string()];
        expected.sort();
        assert_eq!(
            call_targets(&files, &edges, "src/lexical.ts", "lexicalCaller"),
            expected,
            "{edges:#?}"
        );
    }

    /// nw-113 (watcher parity). A nested `function` declaration carries no
    /// declarator width, but it still shadows a module import inside its
    /// enclosing function. Resolved alone (import source absent) the call
    /// must bind it, exactly as the full index does with the source present.
    #[test]
    fn a_nested_function_shadows_an_import_whose_source_is_absent() {
        let lib = ("src/lib.ts", "export function f(): number { return 7; }\n");
        let nested = (
            "src/nested.ts",
            r#"import { f } from "./lib";
export function g() {
  function f() { return 1; }
  return f();
}
"#,
        );
        for inputs in [&[nested][..], &[lib, nested][..]] {
            let (files, edges) = review_edges(inputs, Language::TypeScript);
            assert_eq!(
                call_targets(&files, &edges, "src/nested.ts", "g"),
                vec![uid_at(&files, "src/nested.ts", "f", 3)],
                "{edges:#?}"
            );
        }
    }

    /// nw-113 (review of the nested-declaration rule). With an npm import of
    /// `f` (never resolvable), a same-file `function f` binds a bare call only
    /// when the innermost function or method enclosing `f` also contains the
    /// call. `Some(line)` expects the `f` declared on that line; `None`
    /// expects only `unresolved:f`.
    #[test]
    fn a_nested_function_shadows_an_npm_import_only_from_its_innermost_function() {
        let cases: [(&str, &str, &str, Option<u32>); 11] = [
            // S3: nested in g, called in g.
            (
                "S3",
                "function g() {\n  function f() { return 1; }\n  return f();\n}\n",
                "g",
                Some(3),
            ),
            // Module-level f: the import cannot coexist with it, and it is
            // not nested, so the unresolved import keeps the call.
            (
                "module-level",
                "function f() { return 1; }\nfunction g() {\n  return f();\n}\n",
                "g",
                None,
            ),
            // S4: f is nested in a separate top-level function h.
            (
                "S4",
                "function h() {\n  function f() {}\n}\nfunction g() {\n  return f();\n}\n",
                "g",
                None,
            ),
            // A1 (nw-787): f's region is the anonymous callback's body, even
            // though the callback is not a symbol, so the call in g is
            // outside it.
            (
                "A1",
                "function g() {\n  [1].forEach(() => {\n    function f() {}\n  });\n  return f();\n}\n",
                "g",
                None,
            ),
            // F1: f is visible only inside its sibling h.
            (
                "F1",
                "function outer() {\n  function h() {\n    function f() { return 1; }\n  }\n  function g() {\n    return f();\n  }\n}\n",
                "g",
                None,
            ),
            // F2: f is local to the arrow callback.
            (
                "F2",
                "function g() {\n  const cb = () => {\n    function f() { return 1; }\n  };\n  return f();\n}\n",
                "g",
                None,
            ),
            // F3: a bare call never reaches a method.
            (
                "F3",
                "class C {\n  f() { return 1; }\n  m() {\n    return f();\n  }\n}\n",
                "m",
                None,
            ),
            // F6: f and g are siblings in outer, which contains the call.
            (
                "F6",
                "function outer() {\n  function f() { return 1; }\n  function g() {\n    return f();\n  }\n}\n",
                "g",
                Some(3),
            ),
            // F7: f nested in method m, called in m.
            (
                "F7",
                "class C {\n  m() {\n    function f() { return 1; }\n    return f();\n  }\n}\n",
                "m",
                Some(4),
            ),
            // I2: an earlier sibling's nested homonym must not hide g's own.
            (
                "sibling-homonym",
                "function h() {\n  function f() { return 1; }\n  return 0;\n}\nfunction g() {\n  function f() { return 2; }\n  return f();\n}\n",
                "g",
                Some(7),
            ),
            // F4 (nw-787): a block-nested function declaration is visible only
            // in its block in strict code (every TS file).
            (
                "F4",
                "function g() {\n  {\n    function f() { return 1; }\n  }\n  return f();\n}\n",
                "g",
                None,
            ),
        ];
        for (label, body, caller, expected) in cases {
            let source = format!("import {{ f }} from \"react\";\n{body}");
            let (files, edges) = review_edges(&[("src/app.ts", &source)], Language::TypeScript);
            let expected = match expected {
                Some(line) => vec![uid_at(&files, "src/app.ts", "f", line)],
                None => vec!["unresolved:f".to_string()],
            };
            // Only the calls to `f` (A1's `forEach` call is not under test).
            let targets: Vec<String> = call_targets(&files, &edges, "src/app.ts", caller)
                .into_iter()
                .filter(|target| !target.starts_with("unresolved:") || target == "unresolved:f")
                .collect();
            assert_eq!(targets, expected, "{label}: {edges:#?}");
        }
    }

    /// nw-113 (full index). An npm import never resolves to a file, so a
    /// block-local declarator shadowing it binds inside its block, and the
    /// call after the block stays unresolved.
    #[test]
    fn a_block_local_callable_shadows_an_npm_import_inside_its_block() {
        let app = (
            "src/app.ts",
            r#"import { useState } from "react";
export function component() {
  {
    const useState = () => 0;
    useState();
  }
  return useState();
}
"#,
        );
        let (files, edges) = review_edges(&[app], Language::TypeScript);
        let mut expected = vec![
            uid_at(&files, "src/app.ts", "useState", 4),
            "unresolved:useState".to_string(),
        ];
        expected.sort();
        assert_eq!(
            call_targets(&files, &edges, "src/app.ts", "component"),
            expected,
            "{edges:#?}"
        );
    }

    /// nw-113 (block scope). Sibling and nested blocks: each call binds the
    /// innermost declaration whose block contains it, never a sibling's.
    #[test]
    fn same_file_callables_bind_the_innermost_containing_declaration() {
        let lib = (
            "src/lib.ts",
            "export function sendPacket(): number { return 7; }\n",
        );
        let scopes = (
            "src/scopes.ts",
            r#"import { sendPacket } from "./lib";
export function siblings() {
  {
    const sendPacket = () => 1;
  }
  {
    return sendPacket();
  }
}
export function nested() {
  const sendPacket = () => 2;
  {
    const sendPacket = () => 3;
    sendPacket();
  }
  return sendPacket();
}
export function multi() {
  {
    const first = 1,
      sendPacket = () => 5;
  }
  return sendPacket();
}
"#,
        );
        let (files, edges) = review_edges(&[lib, scopes], Language::TypeScript);
        let imported = uid(&files, "src/lib.ts", "sendPacket");
        let outer = uid_at(&files, "src/scopes.ts", "sendPacket", 11);
        let inner = uid_at(&files, "src/scopes.ts", "sendPacket", 13);

        assert_eq!(
            call_targets(&files, &edges, "src/scopes.ts", "siblings"),
            vec![imported],
            "a sibling block's declaration is not visible"
        );
        let mut expected = vec![outer, inner];
        expected.sort();
        assert_eq!(
            call_targets(&files, &edges, "src/scopes.ts", "nested"),
            expected,
            "inner call binds the inner declaration, outer call the function-level one"
        );
        assert_eq!(
            call_targets(&files, &edges, "src/scopes.ts", "multi"),
            vec![uid(&files, "src/lib.ts", "sendPacket")],
            "a declarator on a later line of its declaration is still block-scoped"
        );
    }

    /// nw-113 (block scope). A name declared in this file only inside a block
    /// that does not contain the call is unresolved: an ES module cannot see a
    /// sibling file's function without importing it, so the same-directory
    /// fallback must not bind it.
    #[test]
    fn an_out_of_scope_block_local_does_not_fall_back_to_a_sibling_file() {
        let a = (
            "src/a.ts",
            r#"export function caller() {
  {
    const helper = () => 1;
  }
  return helper();
}
"#,
        );
        let b = ("src/b.ts", "export function helper() { return 2; }\n");
        let (files, edges) = review_edges(&[a, b], Language::TypeScript);
        let sibling = uid(&files, "src/b.ts", "helper");
        let targets = call_targets(&files, &edges, "src/a.ts", "caller");
        assert_eq!(targets, vec!["unresolved:helper".to_string()]);
        assert!(
            !edges.iter().any(|edge| edge.target_uid == sibling),
            "{edges:#?}"
        );
    }

    /// nw-113 counterweight (code review). A `function` declaration has no
    /// declarator binding of its own, so a same-named local declared inside
    /// its body must not scope the function to that body.
    #[test]
    fn a_function_declaration_is_not_scoped_by_a_same_named_local_in_its_body() {
        let body = r#"function format(x: number) {
  const format = new Intl.NumberFormat();
  return format.format(x);
}
"#;
        let lib = ("src/lib.ts", "export function other() { return 1; }\n");
        let with_import = format!(
            "import {{ other }} from \"./lib\";\n{body}export function caller() {{ other(); return format(1); }}\n"
        );
        let (files, edges) = review_edges(&[lib, ("src/a.ts", &with_import)], Language::TypeScript);
        let mut expected = vec![
            uid(&files, "src/lib.ts", "other"),
            uid_at(&files, "src/a.ts", "format", 2),
        ];
        expected.sort();
        assert_eq!(call_targets(&files, &edges, "src/a.ts", "caller"), expected);

        let without_import = format!("{body}export function caller() {{ return format(1); }}\n");
        let (files, edges) = review_edges(&[("src/a.ts", &without_import)], Language::TypeScript);
        assert_eq!(
            call_targets(&files, &edges, "src/a.ts", "caller"),
            vec![uid_at(&files, "src/a.ts", "format", 1)]
        );

        // Receiver-gate case (not the declaration-form check): `c.items()` has
        // a receiver, so the declaration-scope filter never sees it.
        let method = "class C { items() { const items = [1]; return items; } }\nexport function useIt(c: C) { return c.items(); }\n";
        let (files, edges) = review_edges(&[("src/c.ts", method)], Language::TypeScript);
        assert_eq!(
            call_targets(&files, &edges, "src/c.ts", "useIt"),
            vec![uid(&files, "src/c.ts", "items")],
            "a receiver call to a method is not filtered by block scope"
        );

        // `async` on its own line: the symbol's signature starts at `function`.
        let split_modifier = "async\nfunction format(x: number) {\n  const format = new Intl.NumberFormat();\n  return format.format(x);\n}\nexport function caller() { return format(1); }\n";
        let (files, edges) = review_edges(&[("src/s.ts", split_modifier)], Language::TypeScript);
        assert_eq!(
            call_targets(&files, &edges, "src/s.ts", "caller"),
            vec![uid_at(&files, "src/s.ts", "format", 2)]
        );
    }

    /// nw-113 (code review 2): an assignment-form function (`exports.f =`,
    /// `module.exports.f =`, `obj.f =`) has no declarator of its own, so a
    /// same-named local inside its body must not scope it. The parser mints
    /// no symbol for `obj.f = function` (only CommonJS exports, nw-687), so
    /// that case is unresolved on base too; it is pinned unchanged.
    #[test]
    fn assignment_form_functions_are_not_scoped_by_a_same_named_inner_local() {
        for (path, target, has_symbol) in [
            ("src/exp.js", "exports.format = function (x) {", true),
            ("src/mod.js", "module.exports.format = function (x) {", true),
            ("src/obj.js", "obj.format = function (x) {", false),
        ] {
            let source = format!(
                "const obj = {{}};\n{target}\n  const format = new Intl.NumberFormat();\n  return format.format(x);\n}};\nfunction caller() {{ return format(1); }}\n"
            );
            let (files, edges) = review_edges(&[(path, &source)], Language::JavaScript);
            let expected: Vec<String> = files[0]
                .1
                .iter()
                .filter(|symbol| symbol.name == "format" && symbol.kind == SymbolKind::Function)
                .map(|symbol| symbol_uid("repo:test:abc", path, "format", symbol.start_line))
                .collect();
            let expected = if has_symbol {
                assert_eq!(expected.len(), 1, "{path}: {:#?}", files[0].1);
                expected
            } else {
                assert!(expected.is_empty(), "{path}: {:#?}", files[0].1);
                vec!["unresolved:format".to_string()]
            };
            assert_eq!(
                call_targets(&files, &edges, path, "caller"),
                expected,
                "{path}: {:#?}",
                files[0].1
            );
        }
    }

    /// nw-113 (code review 2): a module-level declarator is visible everywhere
    /// and carries no shadowing information, so it must not outrank a nested
    /// `function` declaration that the call actually reaches.
    #[test]
    fn a_module_level_declarator_does_not_outrank_a_nested_function_declaration() {
        let source = "// header\nfunction g() {\n  function f() { return 1; }\n  return f();\n}\nconst f = () => 2;\n";
        let (files, edges) = review_edges(&[("src/n.ts", source)], Language::TypeScript);
        assert_eq!(
            call_targets(&files, &edges, "src/n.ts", "g"),
            vec![uid_at(&files, "src/n.ts", "f", 3)]
        );
    }

    /// nw-113 (code review 3): module level is decided by symbol nesting, not
    /// by which bindings happen to exist. A declarator inside `g` (directly or
    /// in a block) is scoped to `g` even when nothing outside `g` binds a name,
    /// so it outranks the unscoped module `function f`.
    #[test]
    fn a_function_local_declarator_outranks_a_module_function_declaration() {
        // Multi-line: a call on the same line as a one-line arrow is credited
        // to the arrow (the known caller-by-line residual).
        for (path, source, line) in [
            (
                "src/m1.ts",
                "function f() { return 1; }\nexport function g() {\n  const f = () => 2;\n  return f();\n}\n",
                3,
            ),
            (
                "src/m2.ts",
                "function f() { return 1; }\nexport function g() {\n  {\n    const f = () => 2;\n    return f();\n  }\n}\n",
                4,
            ),
        ] {
            let (files, edges) = review_edges(&[(path, source)], Language::TypeScript);
            assert_eq!(
                call_targets(&files, &edges, path, "g"),
                vec![uid_at(&files, path, "f", line)],
                "{path}"
            );
        }
    }

    #[test]
    fn declarator_form_recognises_declaration_keywords_after_modifiers() {
        for signature in [
            "const f = () => 1;",
            "export const f = () => 1;",
            "let f = () => 1;",
            "var f = function () {};",
            "using f = open();",
            "await using f = open();",
            "export declare const f: () => void;",
        ] {
            assert!(is_declarator_form(signature), "{signature}");
        }
        for signature in [
            "function f() {}",
            "async function f() {}",
            "exports.f = function () {}",
            "obj.f = function () {}",
            "f() {}",
        ] {
            assert!(!is_declarator_form(signature), "{signature}");
        }
    }

    /// nw-787 (was a documented nw-113 residual). A module-level `const f`
    /// sharing line 1 with `function h` reads as scoped (containment is per
    /// line) with the whole module's width, but `g`'s nested `function f` now
    /// carries its body's narrower byte region, so the innermost wins.
    #[test]
    fn a_nested_function_declaration_outranks_a_same_line_module_declarator() {
        let source = "function h() { return 0; } const f = () => 2;\nfunction g() {\n  function f() { return 1; }\n  return f();\n}\n";
        let (files, edges) = review_edges(&[("src/n1.ts", source)], Language::TypeScript);
        assert_eq!(
            call_targets(&files, &edges, "src/n1.ts", "g"),
            vec![uid_at(&files, "src/n1.ts", "f", 3)]
        );
    }

    /// nw-113 (code review): a declarator's own scope is the widest same-named
    /// declarator binding in its span, not the earliest. An earlier
    /// declarator's local `f` must not scope the later `f`. One line on
    /// purpose: a later-line declarator called within its own scope is
    /// already unresolved on main (`name_is_locally_bound` joins a binding to
    /// its symbol by start line), and `let`/an object method on purpose: an
    /// inner `const f` would share the symbol's uid and replace it.
    #[test]
    fn a_declarator_is_scoped_by_its_own_binding_not_an_earlier_inner_local() {
        let source = "let a = { g() { let f = 1; return f; } }, f = () => 2;\nexport function readIt() {\n  return f();\n}\n";
        let (files, edges) = review_edges(&[("src/d.ts", source)], Language::TypeScript);
        let f = files[0]
            .1
            .iter()
            .find(|symbol| symbol.name == "f" && symbol.kind == SymbolKind::Function)
            .unwrap();
        assert_eq!(
            call_targets(&files, &edges, "src/d.ts", "readIt"),
            vec![symbol_uid("repo:test:abc", "src/d.ts", "f", f.start_line)]
        );
    }

    /// nw-113 counterweights (code review): unchanged from main. A receiver
    /// call is never filtered by a block-declared same-file candidate, and a
    /// parameter binding never counts as a declarator.
    #[test]
    fn block_scope_filter_ignores_receiver_calls_and_parameter_bindings() {
        let receiver = (
            "src/r.ts",
            r#"export function make() {
  {
    const get = () => 1;
  }
}
export function use(o: any) {
  return o.get();
}
"#,
        );
        let parameter = (
            "src/p.ts",
            r#"function value(value: number) {
  return value;
}
export function readIt() {
  return value();
}
"#,
        );
        let (files, edges) = review_edges(&[receiver, parameter], Language::TypeScript);
        assert_eq!(
            call_targets(&files, &edges, "src/r.ts", "use"),
            vec!["unresolved:get".to_string()],
            "a receiver call is not filtered by block scope"
        );
        // Receiver-gate case (discriminating): `this` matches any same-file
        // candidate, so the block-declared `get` is bound today. The
        // declaration-scope filter must not reject it.
        let this_call = (
            "src/t.ts",
            "export function use() {\n  {\n    const get = () => 1;\n  }\n  return this.get();\n}\n",
        );
        let (this_files, this_edges) = review_edges(&[this_call], Language::TypeScript);
        assert_eq!(
            call_targets(&this_files, &this_edges, "src/t.ts", "use"),
            vec![uid(&this_files, "src/t.ts", "get")],
            "a `this` receiver call keeps its same-file candidate"
        );
        assert_eq!(
            call_targets(&files, &edges, "src/p.ts", "readIt"),
            vec![uid(&files, "src/p.ts", "value")],
            "a same-named parameter is not a declarator"
        );
        // A CommonJS function is not a declarator, so its same-named
        // parameter (the only in-span binding) never scopes it.
        let commonjs = (
            "src/cjs.js",
            "exports.value = function (value) { return value; };\nfunction readIt() { return value(); }\n",
        );
        let (cjs_files, cjs_edges) = review_edges(&[commonjs], Language::JavaScript);
        assert_eq!(
            call_targets(&cjs_files, &cjs_edges, "src/cjs.js", "readIt"),
            vec![uid(&cjs_files, "src/cjs.js", "value")],
            "a parameter binding does not scope a symbol"
        );
    }

    /// Counterweight: a module-scope callable still wins over any import
    /// lookup, and a function declared elsewhere in the file still resolves
    /// same-file (no LocalBinding exists for a `function` declaration).
    #[test]
    fn module_scope_same_file_callables_still_resolve_first() {
        let source = r#"const helper = () => 1;
function declared() { return 2; }
export function caller() {
  helper();
  return declared();
}
"#;
        let (files, edges) = review_edges(&[("src/m.ts", source)], Language::TypeScript);
        let mut expected = vec![
            uid(&files, "src/m.ts", "helper"),
            uid(&files, "src/m.ts", "declared"),
        ];
        expected.sort();
        assert_eq!(call_targets(&files, &edges, "src/m.ts", "caller"), expected);
    }

    /// Counterweights from plan validation: the filter must not reject
    /// (a) a receiver call into a namespace body (a `statement_block`), or
    /// (b) a symbol whose declaration line also declares a same-named
    /// parameter. The same block as one line must behave like the multiline one.
    #[test]
    fn block_scope_filter_spares_receivers_parameters_and_one_line_blocks() {
        let namespace = (
            "src/api.ts",
            r#"export namespace Api {
  export const get = () => 1;
}
export function useApi() {
  return Api.get();
}
"#,
        );
        let setter = (
            "src/box.ts",
            r#"export class Box {
  set value(value: number) { this.v = value; }
  v = 0;
}
function value() { return 2; }
export function readIt() { return value(); }
"#,
        );
        let lib = (
            "src/lib.ts",
            "export function sendPacket(): number { return 7; }\n",
        );
        let one_line = (
            "src/oneline.ts",
            "import { sendPacket } from \"./lib\";\nexport function lexicalCaller() {\n  { const sendPacket = () => 42; sendPacket(); }\n  return sendPacket();\n}\n",
        );
        let (files, edges) =
            review_edges(&[namespace, setter, lib, one_line], Language::TypeScript);
        let before = |path: &str, caller: &str| call_targets(&files, &edges, path, caller);
        // Known residual (tracked separately): the parser emits no symbol for a TS namespace, so Api.get() is unresolved today. This test only pins that nw-113 leaves it unchanged.
        assert_eq!(
            before("src/api.ts", "useApi"),
            vec!["unresolved:get".to_string()],
            "a receiver call into a namespace body is not touched by the block-scope filter"
        );
        // nw-789: a bare call never binds the setter; the free function wins.
        assert_eq!(
            before("src/box.ts", "readIt"),
            vec![uid_at(&files, "src/box.ts", "value", 5)],
            "a bare call binds the free function, never the setter"
        );
        // Known residual (tracked on nw-113): the caller is chosen by LINE, so a call on the same line as a one-line arrow declaration is credited to the arrow (a sendPacket -> sendPacket self-edge), not to lexicalCaller. This assertion pins the nw-113 fix itself: the call AFTER the block binds the import, not the block-local.
        assert_eq!(
            before("src/oneline.ts", "lexicalCaller"),
            vec![uid(&files, "src/lib.ts", "sendPacket")]
        );
        let one_line_local = uid_at(&files, "src/oneline.ts", "sendPacket", 3);
        assert_eq!(
            before("src/oneline.ts", "sendPacket"),
            vec![one_line_local],
            "today's same-line self-edge; a caller-attribution fix must update this deliberately"
        );
    }

    #[test]
    fn review_unresolved_exports_cannot_prove_a_star_target() {
        let mut failures = Vec::new();
        for (barrel, expected) in [
            ("export * from './known';\n", true),
            (
                "export { helper } from './missing';\nexport * from './known';\n",
                false,
            ),
            (
                "export * from './known';\nexport * from './missing';\n",
                false,
            ),
            (
                "export * from './missing';\nexport * from './known';\n",
                false,
            ),
        ] {
            let (files, edges) = review_edges(
                &[
                    ("src/known.ts", "export function helper() { return 1; }\n"),
                    ("src/index.ts", barrel),
                    (
                        "src/user.ts",
                        "import { helper } from './index';\nfunction user() { return helper(); }\nfunction shadow(helper: () => number) { return helper(); }\n",
                    ),
                ],
                Language::TypeScript,
            );
            let expected = if expected {
                std::collections::BTreeSet::from([uid(&files, "src/user.ts", "user")])
            } else {
                std::collections::BTreeSet::new()
            };
            let actual = review_callers(&files, &edges, "src/known.ts", "helper");
            if actual != expected {
                failures.push(format!(
                    "{barrel}: expected={expected:?} actual={actual:?} edges={edges:#?}"
                ));
            }
        }
        assert!(failures.is_empty(), "{}", failures.join("\n"));
    }

    #[test]
    fn review_imported_receiver_shape_preserves_exposed_members_only() {
        let mut failures = Vec::new();
        for (provider, member, call, expected) in [
            (
                "export class Logger {\n static write() { return 1; }\n}\n",
                "write",
                "api.write()",
                true,
            ),
            (
                "export class Logger {\n write() { return 1; }\n}\n",
                "write",
                "api.write()",
                false,
            ),
            (
                "export class Logger {\n private static write() { return 1; }\n}\n",
                "write",
                "api.write()",
                false,
            ),
            (
                "export default class Logger {\n static write() { return 1; }\n}\n",
                "write",
                "api.write()",
                true,
            ),
            (
                "const db = {\n query() { return 1; }\n};\nexport default db;\n",
                "query",
                "api.query()",
                true,
            ),
            (
                "class Database {\n query() { return 1; }\n}\nconst db = new Database();\nexport default db;\n",
                "query",
                "api.query()",
                true,
            ),
            (
                "class Database {\n private query() { return 1; }\n}\nconst db = new Database();\nexport default db;\n",
                "query",
                "api.query()",
                false,
            ),
            (
                "class Database {\n query() { return 1; }\n}\nconst db = unknownFactory();\nexport default db;\n",
                "query",
                "api.query()",
                false,
            ),
            (
                "const db = {\n query() { return 1; },\n ...unknownMembers\n};\nexport default db;\n",
                "query",
                "api.query()",
                false,
            ),
            (
                "const db = {\n query() { return 1; },\n [unknownKey]() { return 2; }\n};\nexport default db;\n",
                "query",
                "api.query()",
                false,
            ),
        ] {
            let import = if provider.starts_with("export class") {
                "import { Logger as api } from './provider';"
            } else {
                "import api from './provider';"
            };
            let user = format!(
                "{import}\nfunction user() {{ return {call}; }}\nfunction shadow(api: unknown) {{ return {call}; }}\n"
            );
            let (files, edges) = review_edges(
                &[
                    ("src/provider.ts", provider),
                    ("src/user.ts", &user),
                    (
                        "src/unrelated.ts",
                        "export class Database {\n query() { return 3; }\n}\nexport class Logger {\n static write() { return 3; }\n}\n",
                    ),
                ],
                Language::TypeScript,
            );
            let expected = if expected {
                std::collections::BTreeSet::from([uid(&files, "src/user.ts", "user")])
            } else {
                std::collections::BTreeSet::new()
            };
            if provider.contains("export default db") {
                let exposed = files[0]
                    .1
                    .iter()
                    .find(|symbol| symbol.name == member)
                    .unwrap();
                assert!(
                    !exposed.is_entry_point,
                    "exported methods need independent root evidence: {provider}: {exposed:#?}"
                );
            }
            let actual = review_callers(&files, &edges, "src/provider.ts", member);
            let unrelated = review_callers(&files, &edges, "src/unrelated.ts", member);
            if actual != expected || !unrelated.is_empty() {
                failures.push(format!("{provider}: expected={expected:?} actual={actual:?} unrelated={unrelated:?} files={files:#?} edges={edges:#?}"));
            }
        }
        assert!(failures.is_empty(), "{}", failures.join("\n"));
    }

    /// nw-113 (ownerless): a top-level call is owned by the file's `<module>`,
    /// and the call still yields its IMPORTS by-product.
    #[test]
    fn a_top_level_call_is_owned_by_the_module() {
        let lib = (
            "src/lib.ts",
            "export function flushBuffer(): number { return 8; }\n",
        );
        let ownerless = (
            "src/ownerless.ts",
            "import { flushBuffer } from \"./lib\";\nflushBuffer();\n",
        );
        let (files, edges) = review_edges(&[lib, ownerless], Language::TypeScript);
        let owner = nestweaver_schema::MODULE_OWNER_NAME;
        assert_eq!(
            call_targets(&files, &edges, "src/ownerless.ts", owner),
            vec![uid(&files, "src/lib.ts", "flushBuffer")],
            "{edges:#?}"
        );
        let owner_uid = uid(&files, "src/ownerless.ts", owner);
        assert!(
            edges
                .iter()
                .any(|edge| edge.source_uid == owner_uid && edge.edge_type == EdgeType::Imports),
            "{edges:#?}"
        );
    }

    /// Validation case: a one-line function above the call used to become
    /// its fabricated caller through the degenerate fallback.
    #[test]
    fn a_one_line_function_above_a_top_level_call_does_not_own_it() {
        let lib = (
            "src/lib.ts",
            "export function flushBuffer(): number { return 8; }\n",
        );
        let file = (
            "src/oneline.ts",
            "import { flushBuffer } from \"./lib\";\nexport const add = (a: number, b: number) => a + b;\nflushBuffer();\n",
        );
        let (files, edges) = review_edges(&[lib, file], Language::TypeScript);
        let target = vec![uid(&files, "src/lib.ts", "flushBuffer")];
        assert_eq!(
            call_targets(
                &files,
                &edges,
                "src/oneline.ts",
                nestweaver_schema::MODULE_OWNER_NAME
            ),
            target,
            "{edges:#?}"
        );
        assert!(call_targets(&files, &edges, "src/oneline.ts", "add").is_empty());
    }

    /// E2: a one-line function called at top level is called by the module,
    /// not by itself.
    #[test]
    fn a_top_level_call_of_a_one_line_function_is_not_a_self_edge() {
        let file = ("src/e2.ts", "function helper() {}\nhelper();\n");
        let (files, edges) = review_edges(&[file], Language::TypeScript);
        let helper = uid(&files, "src/e2.ts", "helper");
        assert_eq!(
            call_targets(
                &files,
                &edges,
                "src/e2.ts",
                nestweaver_schema::MODULE_OWNER_NAME
            ),
            vec![helper.clone()],
            "{edges:#?}"
        );
        assert!(
            call_targets(&files, &edges, "src/e2.ts", "helper").is_empty(),
            "{edges:#?}"
        );
    }

    /// E7: Python's `__main__` guard is module code; the module calls `main`.
    #[test]
    fn a_python_main_guard_call_is_owned_by_the_module() {
        let file = (
            "app.py",
            "def main():\n    pass\n\nif __name__ == \"__main__\":\n    main()\n",
        );
        let (files, edges) = review_edges(&[file], Language::Python);
        assert_eq!(
            call_targets(
                &files,
                &edges,
                "app.py",
                nestweaver_schema::MODULE_OWNER_NAME
            ),
            vec![uid(&files, "app.py", "main")],
            "{edges:#?}"
        );
    }

    /// Counterweights: calls inside functions keep their function owner, and
    /// non-call references at module scope (a type annotation) get NO edge
    /// from the owner.
    #[test]
    fn the_owner_takes_only_top_level_calls() {
        let lib = (
            "src/lib.ts",
            "export function a(): number { return 1; }\nexport interface User { id: number }\n",
        );
        let mixed = (
            "src/mixed.ts",
            "import { a, User } from \"./lib\";\nexport function f() {\n  return a();\n}\nconst u: User = { id: 1 };\na();\n",
        );
        let (files, edges) = review_edges(&[lib, mixed], Language::TypeScript);
        let target = vec![uid(&files, "src/lib.ts", "a")];
        assert_eq!(call_targets(&files, &edges, "src/mixed.ts", "f"), target);
        let owner = uid(&files, "src/mixed.ts", nestweaver_schema::MODULE_OWNER_NAME);
        let owner_edges: Vec<_> = edges.iter().filter(|e| e.source_uid == owner).collect();
        assert!(
            owner_edges
                .iter()
                .all(|e| matches!(e.edge_type, EdgeType::Calls | EdgeType::Imports)),
            "the owner must not collect Uses/Accesses: {owner_edges:?}"
        );
        let user = uid(&files, "src/lib.ts", "User");
        assert!(
            !owner_edges.iter().any(|e| e.target_uid == user),
            "a module-level type reference is not owned by <module>: {owner_edges:?}"
        );
        assert_eq!(
            call_targets(
                &files,
                &edges,
                "src/mixed.ts",
                nestweaver_schema::MODULE_OWNER_NAME
            ),
            target
        );
    }

    /// E1: the whole-file owner is never a container. With a top-level call
    /// present (so the owner exists), the nested `f` is still `g`'s binding
    /// and the module-level declarator `f` is still module level.
    #[test]
    fn the_module_owner_is_not_a_declaration_container() {
        let file = (
            "src/e1.ts",
            "function g() {\n  function f() { return 1; }\n  return f();\n}\nconst f = () => 2;\nboot();\n",
        );
        let (files, edges) = review_edges(&[file], Language::TypeScript);
        assert!(
            files[0]
                .1
                .iter()
                .any(|s| s.name == nestweaver_schema::MODULE_OWNER_NAME),
            "fixture must mint the owner"
        );
        assert_eq!(
            call_targets(&files, &edges, "src/e1.ts", "g"),
            vec![uid_at(&files, "src/e1.ts", "f", 2)],
            "{edges:#?}"
        );
    }

    /// Express-style registration: a call inside an anonymous callback at
    /// module scope has no named container, so the module owns it.
    #[test]
    fn a_call_in_a_module_level_callback_is_owned_by_the_module() {
        let lib = (
            "src/lib.ts",
            "export function handle(): number { return 1; }\n",
        );
        let app = (
            "src/app.ts",
            "import { handle } from \"./lib\";\napp.get('/', () => {\n  handle();\n});\n",
        );
        let (files, edges) = review_edges(&[lib, app], Language::TypeScript);
        let mut expected = vec![
            uid(&files, "src/lib.ts", "handle"),
            "unresolved:get".to_string(),
        ];
        expected.sort();
        assert_eq!(
            call_targets(
                &files,
                &edges,
                "src/app.ts",
                nestweaver_schema::MODULE_OWNER_NAME
            ),
            expected,
            "{edges:#?}"
        );
    }

    /// PHP: the degenerate guess (a one-line function above a top-level call)
    /// becomes the module owner, not a self-edge.
    #[test]
    fn a_php_top_level_call_after_a_one_line_function_is_owned_by_the_module() {
        let file = (
            "src/boot.php",
            "<?php\nfunction helper() { return 1; }\nhelper();\n",
        );
        let (files, edges) = review_edges(&[file], Language::Php);
        assert_eq!(
            call_targets(
                &files,
                &edges,
                "src/boot.php",
                nestweaver_schema::MODULE_OWNER_NAME
            ),
            vec![uid(&files, "src/boot.php", "helper")],
            "{edges:#?}"
        );
        assert!(
            call_targets(&files, &edges, "src/boot.php", "helper").is_empty(),
            "{edges:#?}"
        );
    }

    /// Non-call references keep today's degenerate guess even when the file
    /// has an owner: a module-level type reference (Uses) and property reads
    /// (Accesses) after a one-line function stay on that function. Pinned so
    /// a later change to the guess is deliberate.
    #[test]
    fn module_level_uses_and_accesses_keep_the_degenerate_guess() {
        let lib = (
            "src/lib.ts",
            "export function a(): number { return 1; }\nexport interface User { id: number }\n",
        );
        let ts = (
            "src/m.ts",
            "import { a, User } from \"./lib\";\nexport function one() { return 1; }\nconst u: User = { id: 1 };\na();\n",
        );
        let (files, edges) = review_edges(&[lib, ts], Language::TypeScript);
        let one = uid(&files, "src/m.ts", "one");
        let user = uid(&files, "src/lib.ts", "User");
        assert!(
            edges.iter().any(|e| e.edge_type == EdgeType::Uses
                && e.source_uid == one
                && e.target_uid == user),
            "{edges:#?}"
        );
        let js = (
            "src/m.js",
            "function one() { return 1; }\nconst c = make();\nconsole.log(c.port);\none();\n",
        );
        let (files, edges) = review_edges(&[js], Language::JavaScript);
        let one = uid(&files, "src/m.js", "one");
        let owner = uid(&files, "src/m.js", nestweaver_schema::MODULE_OWNER_NAME);
        let accesses: Vec<_> = edges
            .iter()
            .filter(|e| e.edge_type == EdgeType::Accesses)
            .collect();
        assert!(!accesses.is_empty(), "{edges:#?}");
        assert!(
            accesses.iter().all(|e| e.source_uid == one),
            "{accesses:#?}"
        );
        assert!(
            edges
                .iter()
                .filter(|e| e.source_uid == owner)
                .all(|e| matches!(e.edge_type, EdgeType::Calls | EdgeType::Imports))
        );
    }

    /// Languages outside the owner gate keep the degenerate guess unchanged:
    /// no `<module>` symbol, and a Lua one-line function still owns the
    /// module-level calls after it.
    #[test]
    fn languages_without_an_owner_keep_the_degenerate_guess() {
        let lua = (
            "boot.lua",
            "local function helper() return 1 end\nprint('x')\nhelper()\n",
        );
        let (files, edges) = review_edges(&[lua], Language::Lua);
        assert!(
            !files[0]
                .1
                .iter()
                .any(|s| s.name == nestweaver_schema::MODULE_OWNER_NAME)
        );
        let helper = uid(&files, "boot.lua", "helper");
        let mut expected = vec![helper.clone(), "unresolved:print".to_string()];
        expected.sort();
        assert_eq!(
            call_targets(&files, &edges, "boot.lua", "helper"),
            expected,
            "{edges:#?}"
        );

        let vue = (
            "src/App.vue",
            "<script>\nfunction helper() { return 1; }\nhelper();\n</script>\n",
        );
        let (files, _) = review_edges(&[vue], Language::Vue);
        assert!(
            !files[0]
                .1
                .iter()
                .any(|s| s.name == nestweaver_schema::MODULE_OWNER_NAME),
            "{:?}",
            files[0].1
        );
    }

    /// nw-113 review: symbols rebuilt from the store (`build_reresolve_edges`)
    /// arrive in DB order, not parser order, so a same-line symbol can precede
    /// the owner. The owner must still sort first and own the call.
    #[test]
    fn the_module_owner_sorts_first_from_symbols_in_db_order() {
        let path = "src/db_order.ts";
        let source = "function f() {}\nf();\n";
        let parsed = parse_source(Path::new(path), source).unwrap();
        let mut symbols = parsed.symbols;
        assert!(symbols[0].is_module_owner(), "{symbols:?}");
        assert_eq!(
            symbols[1].start_line, 1,
            "fixture: f shares the owner's line"
        );
        symbols.swap(0, 1);
        let files = vec![(path.to_string(), symbols, parsed.references)];
        let edges = resolve_references(&files, Language::TypeScript, "repo:test:abc");
        assert_eq!(
            call_targets(&files, &edges, path, nestweaver_schema::MODULE_OWNER_NAME),
            vec![uid(&files, path, "f")],
            "{edges:#?}"
        );
    }

    /// nw-791 (nw-113 B2): calls inside a generator belong to the generator,
    /// and callers of the generator get an edge. No `<module>` owner.
    #[test]
    fn generator_bodies_own_their_calls() {
        let lib = ("src/lib.js", "export function helper(x) { return x; }\n");
        let saga = (
            "src/g.js",
            "import { helper } from './lib';\nexport function* gen(v) { yield helper(v); }\nasync function* agen(v) { yield helper(v); }\nexport function use(v) { return [...gen(v)]; }\n",
        );
        let (files, edges) = review_edges(&[lib, saga], Language::JavaScript);
        let helper = vec![uid(&files, "src/lib.js", "helper")];
        assert_eq!(
            call_targets(&files, &edges, "src/g.js", "gen"),
            helper,
            "{edges:#?}"
        );
        assert_eq!(
            call_targets(&files, &edges, "src/g.js", "agen"),
            helper,
            "{edges:#?}"
        );
        assert_eq!(
            call_targets(&files, &edges, "src/g.js", "use"),
            vec![uid(&files, "src/g.js", "gen")],
            "{edges:#?}"
        );
        assert!(
            !files[1].1.iter().any(|s| s.is_module_owner()),
            "{:?}",
            files[1].1
        );
    }

    /// nw-800: a `catch (f)` parameter and a `for (const f of …)` /
    /// `for (const f in …)` / `for (let f = …; …)` loop variable are local
    /// bindings scoped to their clause or loop. A bare `f()` inside one is
    /// local and never binds the same-file `function f`; a bare `f()` after
    /// it, outside that clause or loop, still does.
    #[test]
    fn catch_and_loop_variables_are_local_to_their_clause() {
        for (path, language) in [
            ("src/clauses.js", Language::JavaScript),
            ("src/clauses.ts", Language::TypeScript),
        ] {
            let source = "function f() {}\n\
                export function viaCatch() { try { work(); } catch (f) { f(); } }\n\
                export function viaForOf(xs) { for (const f of xs) { f(); } }\n\
                export function viaForIn(o) { for (const f in o) { f(); } }\n\
                export function viaFor() { for (let f = make(); f; ) { f(); } }\n\
                export function afterCatch() { try { work(); } catch (f) { log(f); } f(); }\n\
                export function afterForOf(xs) { for (const f of xs) { log(f); } f(); }\n\
                export function afterFor() { for (let f = make(); f; ) { log(f); } f(); }\n";
            let (files, edges) = review_edges(&[(path, source)], language);
            let f = uid(&files, path, "f");
            for caller in ["viaCatch", "viaForOf", "viaForIn", "viaFor"] {
                assert!(
                    !call_targets(&files, &edges, path, caller).contains(&f),
                    "{path}: {caller} must not bind the module f: {edges:#?}"
                );
            }
            for caller in ["afterCatch", "afterForOf", "afterFor"] {
                assert!(
                    call_targets(&files, &edges, path, caller).contains(&f),
                    "{path}: {caller} calls the module f outside the clause: {edges:#?}"
                );
            }
        }
    }

    /// nw-113 final QA round 2 (item 13): an unparenthesised arrow parameter
    /// (`f => f()`, `async f => f()`) is a local binding scoped to the arrow,
    /// exactly like `(f) => f()`. It must not bind the imported `f`, at top
    /// level (a false `<module>` edge) or inside a function or method. A call
    /// after the arrow still binds the import.
    #[test]
    fn bare_arrow_parameters_shadow_imports() {
        for (ext, language) in [("js", Language::JavaScript), ("ts", Language::TypeScript)] {
            let lib = format!("src/lib.{ext}");
            let a = format!("src/a.{ext}");
            let a_source = "import { f, g } from './lib';\n\
                 export const items = [1].map(g => g());\n\
                 [1].forEach(f => {\n\
                 \x20 f();\n\
                 });\n\
                 [2].map(async f => f());\n\
                 export const run = f => f();\n\
                 export class K { cb1() { return [1].map(f => f()); } }\n\
                 export function inner() { return [1].map(g => g()); }\n\
                 export function after() { [1].map(g => g); return g(); }\n";
            let (files, edges) = review_edges(
                &[
                    (&lib, "export function f() {}\nexport function g() {}\n"),
                    (&a, a_source),
                ],
                language,
            );
            let f = uid(&files, &lib, "f");
            let g = uid(&files, &lib, "g");
            let mut callers: Vec<&str> = edges
                .iter()
                .filter(|e| {
                    e.edge_type == EdgeType::Calls && (e.target_uid == f || e.target_uid == g)
                })
                .map(|e| e.source_uid.as_str())
                .collect();
            callers.sort();
            callers.dedup();
            assert_eq!(
                callers,
                vec![uid(&files, &a, "after").as_str()],
                "{a}: only `after` calls the import: {edges:#?}"
            );
        }
    }

    /// nw-113 final review M1: a generator's parameters and `var`s are scoped
    /// to the generator, like any function's. They used to leak to program
    /// scope and shadow a same-named module function everywhere.
    #[test]
    fn generator_parameters_and_vars_stay_inside_the_generator() {
        for (path, language) in [
            ("src/saga.js", Language::JavaScript),
            ("src/saga.ts", Language::TypeScript),
        ] {
            let source = "function* saga(action) { var step = 1; yield action; }\n\
                 const expr = function* (other) { yield other; };\n\
                 function action() {}\n\
                 function step() {}\n\
                 function other() {}\n\
                 export function caller() { return [action(), step(), other()]; }\n";
            let (files, edges) = review_edges(&[(path, source)], language);
            let mut expected = vec![
                uid(&files, path, "action"),
                uid(&files, path, "other"),
                uid(&files, path, "step"),
            ];
            expected.sort();
            assert_eq!(
                call_targets(&files, &edges, path, "caller"),
                expected,
                "{path}: {edges:#?}"
            );
        }
    }

    /// nw-803: ESM imports are hoisted. A function written above the
    /// `import` line sees the imported name, and a parameter of that name
    /// still shadows it.
    #[test]
    fn esm_imports_bind_code_written_above_them() {
        for (lib, a, language) in [
            ("src/lib.ts", "src/a.ts", Language::TypeScript),
            ("src/lib.js", "src/a.js", Language::JavaScript),
        ] {
            let lib_source = "export function helper() { return 1; }\n";
            let a_source = "export function early() { return helper(); }\n\
                 export function shadowed(helper) { return helper(); }\n\
                 import { helper } from './lib';\n\
                 export function late() { return helper(); }\n";
            let (files, edges) = review_edges(&[(lib, lib_source), (a, a_source)], language);
            let helper = vec![uid(&files, lib, "helper")];
            for caller in ["early", "late"] {
                assert_eq!(
                    call_targets(&files, &edges, a, caller),
                    helper,
                    "{a}: {caller}: {edges:#?}"
                );
            }
            assert!(
                !call_targets(&files, &edges, a, "shadowed").contains(&helper[0]),
                "{a}: a parameter shadows the import: {edges:#?}"
            );
        }
    }

    /// nw-113 final review M5: a CommonJS `require` runs where written, but a
    /// function body above it runs later, after the binding is initialized,
    /// so its calls through the binding resolve. Top-level code above the
    /// `require` (including a top-level block) runs first and does not bind.
    #[test]
    fn a_function_above_a_require_sees_the_binding_but_top_level_code_does_not() {
        for (lib, a, language, import) in [
            (
                "src/lib.js",
                "src/a.js",
                Language::JavaScript,
                "const lib = require('./lib');",
            ),
            (
                "src/lib.ts",
                "src/a.ts",
                Language::TypeScript,
                "import lib = require('./lib');",
            ),
        ] {
            let lib_source = "export function helper() { return 1; }\n";
            let a_source = format!(
                "function early() {{ return lib.helper(); }}\n\
                 lib.helper();\n\
                 {{ lib.helper(); }}\n\
                 {import}\n\
                 function late() {{ return lib.helper(); }}\n"
            );
            let (files, edges) = review_edges(&[(lib, lib_source), (a, &a_source)], language);
            let helper = uid(&files, lib, "helper");
            for caller in ["early", "late"] {
                assert_eq!(
                    call_targets(&files, &edges, a, caller),
                    vec![helper.clone()],
                    "{a}: {caller}: {edges:#?}"
                );
            }
            assert!(
                !call_targets(&files, &edges, a, nestweaver_schema::MODULE_OWNER_NAME)
                    .contains(&helper),
                "{a}: top-level code above the require runs first: {edges:#?}"
            );
        }
    }

    /// Timing probe for whole-module bindings (nw-113 item 15). Run with
    /// `--ignored --nocapture`.
    #[test]
    #[ignore]
    fn whole_module_binding_timing_probe() {
        let n: usize = std::env::var("NW_PROBE_N")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(3000);
        type Shape = (&'static str, &'static str, Language, fn(usize) -> String);
        let shapes: [Shape; 3] = [
            ("import =", "src/a.ts", Language::TypeScript, |i| {
                format!("import m{i} = require('./lib');\nexport function q{i}() {{ m{i}(); }}\n")
            }),
            ("import * as", "src/a.ts", Language::TypeScript, |i| {
                format!("import * as m{i} from './lib';\nexport function q{i}() {{ m{i}.f(); }}\n")
            }),
            ("require", "src/a.js", Language::JavaScript, |i| {
                format!("const m{i} = require('./lib');\nfunction q{i}() {{ m{i}.f(); }}\n")
            }),
        ];
        for (label, path, language, line) in shapes {
            let source: String = (0..n).map(line).collect();
            let lib = if language == Language::TypeScript {
                "src/lib.ts"
            } else {
                "src/lib.js"
            };
            let started = std::time::Instant::now();
            let (_files, edges) = review_edges(
                &[(lib, "export function f() {}\n"), (path, &source)],
                language,
            );
            eprintln!(
                "{label} x{n}: {:?} ({} edges)",
                started.elapsed(),
                edges.len()
            );
        }
    }

    /// nw-113 final review Minor 7: the receiver rule in Python.
    /// `helpers.run()` inside a function named `run`, after `import helpers`,
    /// never binds the same-file `run`.
    #[test]
    fn a_receiver_call_never_binds_a_same_file_name_in_python() {
        let (files, edges) = review_edges(
            &[
                ("helpers.py", "def run():\n    return 1\n"),
                (
                    "main.py",
                    "import helpers\n\n\ndef run():\n    return helpers.run()\n",
                ),
            ],
            Language::Python,
        );
        let same_file = uid(&files, "main.py", "run");
        assert!(
            !edges.iter().any(|edge| edge.target_uid == same_file),
            "{edges:#?}"
        );
    }

    /// nw-113 final review Minor 7: the receiver rule in PHP. `X::m()` after
    /// `use Lib\X;` must not bind the same-file `m`, but it does: the parser
    /// records no receiver for a `scoped_call_expression`, so the rule never
    /// sees one. Fixing it changes parser output (a parsed-cache bump) and
    /// every PHP static call's resolution (`self::`, `static::`, `parent::`),
    /// so it is reported rather than fixed in nw-113.
    #[test]
    #[ignore = "PHP static calls carry no receiver; see the doc comment"]
    fn a_receiver_call_never_binds_a_same_file_name_in_php() {
        let (files, edges) = review_edges(
            &[
                (
                    "src/Lib/X.php",
                    "<?php\nnamespace Lib;\nclass X { public static function m() { return 1; } }\n",
                ),
                (
                    "src/main.php",
                    "<?php\nuse Lib\\X;\nfunction m() { return X::m(); }\n",
                ),
            ],
            Language::Php,
        );
        let same_file = uid(&files, "src/main.php", "m");
        assert!(
            !edges.iter().any(|edge| edge.target_uid == same_file),
            "{edges:#?}"
        );
    }

    /// nw-113 final watch parity O3: a member access with a receiver
    /// (`Modal.latestProps`) never falls back to a same-file symbol of the
    /// bare member name, so `latestProps` gets no self-ACCESSES edge. The
    /// same holds for a call through the import (`Modal.render()`).
    #[test]
    fn a_member_access_never_binds_a_same_file_name() {
        for (ext, language) in [("js", Language::JavaScript), ("ts", Language::TypeScript)] {
            let modal = format!("src/Modal.{ext}");
            let test = format!("src/Modal.test.{ext}");
            let (files, edges) = review_edges(
                &[
                    (&modal, "export function Modal(props) { return null; }\n"),
                    (
                        &test,
                        "import { Modal } from './Modal';\nconst latestProps = () => Modal.latestProps;\nexport function render() { return Modal.render(); }\n",
                    ),
                ],
                language,
            );
            for name in ["latestProps", "render"] {
                let same_file = uid(&files, &test, name);
                assert!(
                    !edges.iter().any(|edge| edge.target_uid == same_file),
                    "{test}: {name}: {edges:#?}"
                );
            }
        }
    }

    /// nw-113 final QA round 2 (item 14): an expression-body arrow written
    /// above a CommonJS `require` runs later, like any function body, so its
    /// call binds the required name. Top-level code above the `require`,
    /// including a concise callback, still runs first and does not bind.
    #[test]
    fn a_concise_arrow_above_a_require_sees_the_binding() {
        let lib = ("src/lib.js", "export function f() { return 1; }\n");
        let b = (
            "src/b.js",
            "const fb = () => f();\n\
             const fb2 = () =>\n\
             \x20 f();\n\
             [1].map(() => f());\n\
             const { f } = require('./lib');\n\
             module.exports = { fb, fb2 };\n",
        );
        let c = (
            "src/c.js",
            "function outer() {\n\
             \x20 const fb6 = () => f();\n\
             \x20 const { f } = require('./lib');\n\
             \x20 return fb6;\n\
             }\n\
             module.exports = { outer };\n",
        );
        let (files, edges) = review_edges(&[lib, b, c], Language::JavaScript);
        let f = vec![uid(&files, "src/lib.js", "f")];
        for (path, caller) in [("src/b.js", "fb"), ("src/b.js", "fb2"), ("src/c.js", "fb6")] {
            assert_eq!(
                call_targets(&files, &edges, path, caller),
                f,
                "{path}: {caller}: {edges:#?}"
            );
        }
        assert!(
            !call_targets(
                &files,
                &edges,
                "src/b.js",
                nestweaver_schema::MODULE_OWNER_NAME
            )
            .contains(&f[0]),
            "top-level code above the require runs first: {edges:#?}"
        );
    }

    /// nw-113 final review M2: a receiver call on a `catch` or `for … of`
    /// variable is unresolved, as one on a parameter is (nw-724), even when
    /// the variable shadows a namespace import whose module has the member.
    #[test]
    fn receiver_calls_on_catch_and_loop_variables_are_unresolved() {
        for (ext, language) in [("js", Language::JavaScript), ("ts", Language::TypeScript)] {
            let svc = format!("src/svc.{ext}");
            let e = format!("src/e.{ext}");
            let run = format!("src/run.{ext}");
            let run_source = "import * as svc from './svc';\n\
                 import * as e from './e';\n\
                 export function loop(list) { for (const svc of list) svc.start(); }\n\
                 export function caught() { try { work(); } catch (e) { e.report(); } }\n\
                 export function control() { svc.start(); e.report(); }\n";
            let (files, edges) = review_edges(
                &[
                    (&svc, "export function start() {}\n"),
                    (&e, "export function report() {}\n"),
                    (&run, run_source),
                ],
                language,
            );
            let start = uid(&files, &svc, "start");
            let report = uid(&files, &e, "report");
            let mut control = vec![start.clone(), report.clone()];
            control.sort();
            assert_eq!(
                call_targets(&files, &edges, &run, "control"),
                control,
                "{run}: {edges:#?}"
            );
            assert_eq!(
                call_targets(&files, &edges, &run, "loop"),
                vec!["unresolved:start".to_string()],
                "{run}: {edges:#?}"
            );
            assert!(
                !call_targets(&files, &edges, &run, "caught").contains(&report),
                "{run}: {edges:#?}"
            );
        }
    }

    /// nw-113 final review M3: `export import x = require('…')` binds `x`
    /// like `import x = require('…')`.
    #[test]
    fn ts_export_import_equals_require_binds_like_an_import() {
        let lib = ("src/lib.ts", "export function helper() { return 1; }\n");
        let a = (
            "src/a.ts",
            "export import util = require('./lib');\n\
             export function use() { return util.helper(); }\n",
        );
        let (files, edges) = review_edges(&[lib, a], Language::TypeScript);
        assert_eq!(
            call_targets(&files, &edges, "src/a.ts", "use"),
            vec![uid(&files, "src/lib.ts", "helper")],
            "{edges:#?}"
        );
    }

    /// nw-113 final review M3: calling a whole-module binding bare (`ns()`,
    /// `x()`) calls the module's value, which no export name identifies, so
    /// the call is unresolved and never binds a same-named export.
    #[test]
    fn bare_calls_on_whole_module_bindings_are_unresolved() {
        for (ext, language) in [("js", Language::JavaScript), ("ts", Language::TypeScript)] {
            let ns = format!("src/ns.{ext}");
            let rel = format!("src/rel.{ext}");
            let a = format!("src/a.{ext}");
            let a_source = "import * as ns from './ns';\n\
                 const rel = require('./rel');\n\
                 export function use() { return [ns(), rel()]; }\n";
            let (files, edges) = review_edges(
                &[
                    (&ns, "export function ns() { return 1; }\n"),
                    (&rel, "export function rel() { return 2; }\n"),
                    (&a, a_source),
                ],
                language,
            );
            assert_eq!(
                call_targets(&files, &edges, &a, "use"),
                vec!["unresolved:ns".to_string(), "unresolved:rel".to_string()],
                "{a}: {edges:#?}"
            );
        }
    }

    /// nw-113 final review M3: a hoisted import is visible above its line, and
    /// a block-local or function-local declarator there still shadows it.
    #[test]
    fn a_local_declarator_shadows_a_hoisted_import_above_the_import_line() {
        for (lib, a, language) in [
            ("src/lib.ts", "src/a.ts", Language::TypeScript),
            ("src/lib.js", "src/a.js", Language::JavaScript),
        ] {
            let a_source = "export function local() {\n  const helper = () => 2;\n  return helper();\n}\n\
                 export function block() {\n  {\n    const helper = () => 3;\n    helper();\n  }\n  return helper();\n}\n\
                 import { helper } from './lib';\n";
            let (files, edges) = review_edges(
                &[
                    (lib, "export function helper() { return 1; }\n"),
                    (a, a_source),
                ],
                language,
            );
            let imported = uid(&files, lib, "helper");
            assert_eq!(
                call_targets(&files, &edges, a, "local"),
                vec![uid_at(&files, a, "helper", 2)],
                "{a}: {edges:#?}"
            );
            let mut block = vec![uid_at(&files, a, "helper", 7), imported];
            block.sort();
            assert_eq!(
                call_targets(&files, &edges, a, "block"),
                block,
                "{a}: {edges:#?}"
            );
        }
    }

    /// nw-801: TS `import x = require('…')` binds `x` exactly like
    /// `import * as x from '…'`. A package import leaves `x()` external
    /// instead of falling through to a nested same-named declaration, and a
    /// relative import resolves `x.member()` into that module.
    #[test]
    fn ts_import_equals_require_binds_like_an_import() {
        let lib = ("src/lib.ts", "export function helper() { return 1; }\n");
        let a = (
            "src/a.ts",
            "import debug = require('debug');\n\
             import lib = require('./lib');\n\
             export function run() { function debug(m: string) { return m; } return debug('in'); }\n\
             export function use() { return debug('out') + lib.helper(); }\n",
        );
        let (files, edges) = review_edges(&[lib, a], Language::TypeScript);
        assert_eq!(
            call_targets(&files, &edges, "src/a.ts", "use"),
            vec![
                uid(&files, "src/lib.ts", "helper"),
                "unresolved:debug".to_string()
            ],
            "{edges:#?}"
        );
    }

    /// nw-113 B1: a `.js` arrow with a bare parameter is a function: it owns
    /// its body's calls and its callers get an edge.
    #[test]
    fn bare_parameter_arrows_own_their_calls() {
        let lib = (
            "src/lib.js",
            "export function helper(x) { return x + 1; }\n",
        );
        let a = (
            "src/a.js",
            "import { helper } from './lib';\nconst clamp = (value, min, max) => Math.min(Math.max(value, min), max);\nconst normalizeDirection = value => helper(clamp(Number(value) || 0, 0, 360));\nconst parseDistance = value => { const parsed = helper(value); return parsed; };\nexport function use(v) { return normalizeDirection(v) + parseDistance(v); }\n",
        );
        let (files, edges) = review_edges(&[lib, a], Language::JavaScript);
        let normalize = call_targets(&files, &edges, "src/a.js", "normalizeDirection");
        for target in [
            uid(&files, "src/lib.js", "helper"),
            uid(&files, "src/a.js", "clamp"),
        ] {
            assert!(normalize.contains(&target), "{target} in {normalize:?}");
        }
        let mut uses = vec![
            uid(&files, "src/a.js", "normalizeDirection"),
            uid(&files, "src/a.js", "parseDistance"),
        ];
        uses.sort();
        assert_eq!(
            call_targets(&files, &edges, "src/a.js", "use"),
            uses,
            "{edges:#?}"
        );
        assert!(
            !files[1].1.iter().any(|s| s.is_module_owner()),
            "{:?}",
            files[1].1
        );
    }

    /// nw-113 final review I1: an aliased import below a one-line function is
    /// module scope, not the one-line function's. The degenerate fallback
    /// guesses that function as the binding's owner, while the call's source is
    /// the `<module>` owner, so the binding read as out of scope and the call
    /// had no edge (baseline: `one -> lib.helper`).
    #[test]
    fn an_aliased_python_import_after_a_one_line_function_still_binds() {
        let lib = ("lib.py", "def helper():\n    return 1\n");
        let main = (
            "main3.py",
            "def one(): return 1\nfrom lib import helper as h\nh()\n",
        );
        let (files, edges) = review_edges(&[lib, main], Language::Python);
        assert_eq!(
            call_targets(
                &files,
                &edges,
                "main3.py",
                nestweaver_schema::MODULE_OWNER_NAME
            ),
            vec![uid(&files, "lib.py", "helper")],
            "{edges:#?}"
        );
        // The same import is module scope for a later function too, in a file
        // with no top-level call (so no owner).
        let later = (
            "later.py",
            "def one(): return 1\nfrom lib import helper as h\ndef two():\n    return h()\n",
        );
        let (files, edges) = review_edges(&[lib, later], Language::Python);
        assert_eq!(
            call_targets(&files, &edges, "later.py", "two"),
            vec![uid(&files, "lib.py", "helper")],
            "{edges:#?}"
        );
    }

    /// nw-113 final review I1: an npm import below a one-line arrow is module
    /// scope, so a function nested in `g` still shadows it inside `g`.
    #[test]
    fn a_nested_function_shadows_an_npm_import_below_a_one_line_function() {
        for (path, language, head) in [
            (
                "src/degen.ts",
                Language::TypeScript,
                "export const id = (x: number) => x;\nimport { f } from \"react\";\n",
            ),
            (
                "src/degen.js",
                Language::JavaScript,
                "export const id = (x) => x;\nimport { f } from \"react\";\n",
            ),
            (
                "src/degen_cjs.js",
                Language::JavaScript,
                "const id = (x) => x;\nconst { f } = require(\"react\");\n",
            ),
        ] {
            // Multi-line, so the call's line belongs to `g` alone (one-line
            // code is credited per line, a known residual).
            let source = format!(
                "{head}function g() {{\n  function f() {{ return 1; }}\n  return f();\n}}\n"
            );
            let (files, edges) = review_edges(&[(path, &source)], language);
            assert_eq!(
                call_targets(&files, &edges, path, "g"),
                vec![uid_at(&files, path, "f", 4)],
                "{path}: {edges:#?}"
            );
        }
    }

    /// nw-113 final review I1 (JS): a module-level aliased import below a
    /// one-line arrow binds a top-level call.
    #[test]
    fn an_aliased_js_import_after_a_one_line_function_still_binds() {
        let lib = ("src/lib.js", "export function helper() { return 1; }\n");
        let main = (
            "src/main.js",
            "export const one = () => 1;\nimport { helper as h } from './lib';\nh();\n",
        );
        let (files, edges) = review_edges(&[lib, main], Language::JavaScript);
        assert_eq!(
            call_targets(
                &files,
                &edges,
                "src/main.js",
                nestweaver_schema::MODULE_OWNER_NAME
            ),
            vec![uid(&files, "src/lib.js", "helper")],
            "{edges:#?}"
        );
    }

    /// nw-113 perf (bug-hunt #1): the per-candidate work is bounded. In a file
    /// with N same-named declarators, one candidate's lookup returns exactly
    /// its own declarator (not all N), and the module-level test is one array
    /// read that never counts the `<module>` owner.
    #[test]
    fn file_scope_index_bounds_per_candidate_work() {
        let n = 200u32;
        let mut source: String = (0..n)
            .map(|i| format!("function c{i}() {{ const f = () => {i}; return f(); }}\n"))
            .collect();
        source.push_str("const g = () => 1;\nboot();\n");
        let parsed = parse_source(Path::new("src/big.ts"), &source).unwrap();
        assert!(
            parsed.symbols[0].is_module_owner(),
            "fixture mints an owner"
        );
        let mut syms: Vec<&RawSymbol> = parsed.symbols.iter().collect();
        syms.sort_by_key(|s| (s.start_line, !s.is_module_owner()));
        let bindings: Vec<&RawReference> = parsed
            .references
            .iter()
            .filter(|r| r.kind == ReferenceKind::LocalBinding)
            .collect();
        let index = FileScopeIndex::build(&bindings, &syms);

        let fs: Vec<&&RawSymbol> = syms.iter().filter(|s| s.name == "f").collect();
        assert_eq!(fs.len(), n as usize);
        for f in fs {
            let found = index.declarators_in("f", f.start_line, f.end_line.max(f.start_line));
            assert_eq!(found.len(), 1, "only f@{}'s own declarator", f.start_line);
            assert_eq!(found[0].start_line, f.start_line);
            // Inside c{i}: covered by c{i} and f, so not module level.
            assert_eq!(index.covering(f.start_line), 2);
        }
        // `g` is module level: only g covers its line; the owner never counts.
        let g = syms.iter().find(|s| s.name == "g").unwrap();
        assert_eq!(index.covering(g.start_line), 1);
        assert_eq!(index.covering(n + 2), 0, "the top-level call line");
        assert!(index.declarators_in("missing", 1, n).is_empty());
    }

    /// nw-113 perf repro (bug-hunt #1): N functions in one file, each with
    /// `const f = () => N; return f();`. Ignored; run with `--ignored
    /// --nocapture` to print timings. Not a timing assertion.
    #[test]
    #[ignore]
    fn bench_same_name_declarators_in_one_file() {
        for n in [1000usize, 2000, 3000] {
            let source: String = (0..n)
                .map(|i| format!("function c{i}() {{ const f = () => {i}; return f(); }}\n"))
                .collect();
            let parsed = parse_source(Path::new("src/big.ts"), &source).unwrap();
            let files = vec![("src/big.ts".to_string(), parsed.symbols, parsed.references)];
            let started = std::time::Instant::now();
            let edges = resolve_references(&files, Language::TypeScript, "repo:test:abc");
            eprintln!(
                "BENCH n={n} resolve={:?} edges={}",
                started.elapsed(),
                edges.len()
            );
        }
        // The realistic shape from the bug hunt: 400 components in one file,
        // each with 40 locals and `const render = () => …; return render();`.
        let source: String = (0..400)
            .map(|i| {
                let locals: String = (0..40)
                    .map(|j| format!("  const v{j} = compute{j}({i});\n"))
                    .collect();
                format!(
                    "export function C{i}() {{\n{locals}  const render = () => v0 + v39;\n  return render();\n}}\n"
                )
            })
            .collect();
        let parsed = parse_source(Path::new("src/big.ts"), &source).unwrap();
        let files = vec![("src/big.ts".to_string(), parsed.symbols, parsed.references)];
        let started = std::time::Instant::now();
        let edges = resolve_references(&files, Language::TypeScript, "repo:test:abc");
        eprintln!(
            "BENCH components=400 lines={} resolve={:?} edges={}",
            source.lines().count(),
            started.elapsed(),
            edges.len()
        );
    }

    /// nw-787: the JS/TS nested-function shapes, in the NORMAL resolution
    /// path. Each body is resolved twice: with no import (a same-file name out
    /// of scope is unresolved) and with `import { f } from "./lib"` where
    /// `./lib` is present (an invisible nested `f` yields to the import, a
    /// visible one shadows it). `Some(line)` names the expected same-file `f`
    /// by its line in `body`; `None` expects no same-file `f`.
    #[test]
    fn nested_function_declarations_bind_only_where_visible() {
        let cases: [(&str, &str, &str, Option<u32>); 13] = [
            (
                "F1",
                "function outer() {\n  function h() {\n    function f() { return 1; }\n  }\n  function g() {\n    return f();\n  }\n}\n",
                "g",
                None,
            ),
            (
                "F2",
                "function g() {\n  const cb = () => {\n    function f() { return 1; }\n  };\n  return f();\n}\n",
                "g",
                None,
            ),
            (
                "F3",
                "class C {\n  f() { return 1; }\n  m() {\n    return f();\n  }\n}\n",
                "m",
                None,
            ),
            (
                "F4",
                "function g() {\n  {\n    function f() { return 1; }\n  }\n  return f();\n}\n",
                "g",
                None,
            ),
            (
                "S4",
                "function h() {\n  function f() {}\n}\nfunction g() {\n  return f();\n}\n",
                "g",
                None,
            ),
            (
                "A1 anonymous callback",
                "function g() {\n  [1].forEach(() => {\n    function f() {}\n  });\n  return f();\n}\n",
                "g",
                None,
            ),
            (
                "F6",
                "function outer() {\n  function f() { return 1; }\n  function g() {\n    return f();\n  }\n}\n",
                "g",
                Some(2),
            ),
            (
                "F7",
                "class C {\n  m() {\n    function f() { return 1; }\n    return f();\n  }\n}\n",
                "m",
                Some(3),
            ),
            (
                "sibling-homonym",
                "function h() {\n  function f() { return 1; }\n  return 0;\n}\nfunction g() {\n  function f() { return 2; }\n  return f();\n}\n",
                "g",
                Some(6),
            ),
            (
                "sibling-homonym (methods)",
                "class C {\n  m() {\n    function f() { return 1; }\n    return 0;\n  }\n  n() {\n    function f() { return 2; }\n    return f();\n  }\n}\n",
                "n",
                Some(7),
            ),
            (
                "hoisted call before the declaration",
                "function g() {\n  const x = f();\n  function f() { return 1; }\n  return x;\n}\n",
                "g",
                Some(3),
            ),
            (
                "hoisted call before a block declaration",
                "function g() {\n  {\n    f();\n    function f() { return 1; }\n  }\n}\n",
                "g",
                Some(4),
            ),
            (
                "module-level declaration, file-wide",
                "function g() {\n  const cb = () => {\n    {\n      return f();\n    }\n  };\n  return cb;\n}\nfunction f() { return 1; }\n",
                "cb",
                Some(9),
            ),
        ];
        let lib = ("src/lib.ts", "export function f(): number { return 7; }\n");
        let mut failures = Vec::new();
        for (label, body, caller, expected) in cases {
            for with_import in [false, true] {
                // A module-level `f` and an import of `f` cannot coexist.
                if with_import && label.starts_with("module-level") {
                    continue;
                }
                let source = if with_import {
                    format!("import {{ f }} from \"./lib\";\n{body}")
                } else {
                    body.to_string()
                };
                let inputs: Vec<(&str, &str)> = if with_import {
                    vec![lib, ("src/app.ts", &source)]
                } else {
                    vec![("src/app.ts", &source)]
                };
                let (files, edges) = review_edges(&inputs, Language::TypeScript);
                let want = match (expected, with_import) {
                    (Some(line), _) => uid_at(&files, "src/app.ts", "f", line + with_import as u32),
                    (None, false) => "unresolved:f".to_string(),
                    (None, true) => uid(&files, "src/lib.ts", "f"),
                };
                let f_uids: Vec<String> = files
                    .iter()
                    .flat_map(|(path, symbols, _)| {
                        symbols
                            .iter()
                            .filter(|symbol| symbol.name == "f")
                            .map(move |symbol| {
                                symbol_uid("repo:test:abc", path, "f", symbol.start_line)
                            })
                    })
                    .collect();
                let got: Vec<String> = call_targets(&files, &edges, "src/app.ts", caller)
                    .into_iter()
                    .filter(|target| target == "unresolved:f" || f_uids.contains(target))
                    .collect();
                if got != vec![want.clone()] {
                    failures.push(format!(
                        "{label} (import: {with_import}): want {want}, got {got:?}"
                    ));
                }
            }
        }
        assert!(failures.is_empty(), "{failures:#?}");
    }

    /// nw-787: a block-level `function` is block-scoped in strict code (TS,
    /// an ES module including any `.mjs`/`.mts` file, a `"use strict"`
    /// directive anywhere in the file's or the enclosing function's prologue)
    /// and, per ECMA-262 Annex B.3.3, function-scoped in a sloppy script.
    #[test]
    fn a_block_function_is_block_scoped_only_in_strict_code() {
        let body = "function g() {\n  {\n    function f() { return 1; }\n  }\n  return f();\n}\n";
        let strict_g = "function g() {\n  'use strict';\n  {\n    function f() { return 1; }\n  }\n  return f();\n}\n";
        for (path, prefix, body, line, block_scoped) in [
            ("src/module.js", "export const x = 1;\n", body, 4, true),
            ("src/strict.js", "'use strict';\n", body, 4, true),
            (
                "src/client.js",
                "\"use client\";\n'use strict';\n",
                body,
                5,
                true,
            ),
            ("src/esm.mjs", "", body, 3, true),
            ("src/fn.js", "", strict_g, 4, true),
            ("src/script.js", "", body, 3, false),
            ("src/typed.ts", "", body, 3, true),
        ] {
            let source = format!("{prefix}{body}");
            let language = if path.ends_with(".ts") {
                Language::TypeScript
            } else {
                Language::JavaScript
            };
            let (files, edges) = review_edges(&[(path, &source)], language);
            let want = if block_scoped {
                "unresolved:f".to_string()
            } else {
                uid_at(&files, path, "f", line)
            };
            assert_eq!(
                call_targets(&files, &edges, path, "g"),
                vec![want],
                "{path}"
            );
        }
    }

    /// nw-787 (M1): the graph keys a symbol by name and line, so a nested
    /// `f` on the same line as a module-level `f` is that file-wide symbol: a
    /// call outside `a` still binds it.
    #[test]
    fn a_one_line_nested_homonym_keeps_the_module_level_function_file_wide() {
        let source =
            "function a(){ function f(){} } function f(){}\nfunction g() { return f(); }\n";
        let (files, edges) = review_edges(&[("src/app.js", source)], Language::JavaScript);
        assert_eq!(
            call_targets(&files, &edges, "src/app.js", "g"),
            vec![uid_at(&files, "src/app.js", "f", 1)]
        );
    }

    /// nw-789 (M7): in an import-less file a bare `run()` that rejects the
    /// same-file method `run` must not fall through to a sibling file's
    /// `function run`.
    #[test]
    fn a_bare_call_rejecting_a_same_file_method_invents_no_sibling_edge() {
        for language in [Language::JavaScript, Language::TypeScript] {
            let ext = if language == Language::TypeScript {
                "ts"
            } else {
                "js"
            };
            let (app, lib) = (format!("src/app.{ext}"), format!("src/lib.{ext}"));
            let (files, edges) = review_edges(
                &[
                    (&app, "class C {\n  run() {}\n  m() { run(); }\n}\n"),
                    (&lib, "export function run() {}\n"),
                ],
                language,
            );
            let sibling = uid(&files, &lib, "run");
            assert_eq!(
                call_targets(&files, &edges, &app, "m"),
                vec!["unresolved:run".to_string()],
                "{app}"
            );
            assert!(
                !edges.iter().any(|edge| edge.target_uid == sibling),
                "{edges:#?}"
            );
        }
    }

    /// nw-787 (Python): a `def` nested in a function or method binds a bare
    /// call only inside that function, so a sibling's nested homonym never
    /// binds and never hides the caller's own; a module-level `def` binds
    /// file-wide.
    #[test]
    fn python_nested_defs_bind_only_inside_their_function() {
        let cases: [(&str, &str, &str, Option<u32>); 5] = [
            (
                "sibling-homonym",
                "def h():\n    def f():\n        return 1\n    return 0\n\ndef g():\n    def f():\n        return 2\n    return f()\n",
                "g",
                Some(7),
            ),
            (
                "nested in a sibling",
                "def h():\n    def f():\n        return 1\n    return 0\n\ndef g():\n    return f()\n",
                "g",
                None,
            ),
            (
                "module-level",
                "def g():\n    return f()\n\ndef f():\n    return 1\n",
                "g",
                Some(4),
            ),
            (
                "nested in a method, called there",
                "class C:\n    def m(self):\n        def f():\n            return 1\n        return f()\n\n    def n(self):\n        return f()\n",
                "m",
                Some(3),
            ),
            (
                "nested in a method, called from a sibling method",
                "class C:\n    def m(self):\n        def f():\n            return 1\n        return f()\n\n    def n(self):\n        return f()\n",
                "n",
                None,
            ),
        ];
        let mut failures = Vec::new();
        for (label, source, caller, expected) in cases {
            let (files, edges) = review_edges(&[("app.py", source)], Language::Python);
            let want = match expected {
                Some(line) => uid_at(&files, "app.py", "f", line),
                None => "unresolved:f".to_string(),
            };
            let got = call_targets(&files, &edges, "app.py", caller);
            if got != vec![want.clone()] {
                failures.push(format!("{label}: want {want}, got {got:?}"));
            }
        }
        assert!(failures.is_empty(), "{failures:#?}");
    }

    /// nw-789: a bare JS/TS call never binds a method, getter or setter; a
    /// same-named free function wins, and receiver calls are unchanged.
    #[test]
    fn a_bare_call_never_binds_a_method_getter_or_setter() {
        let source = r#"export class Box {
  get size() { return 1; }
  set value(v: number) { this.v = v; }
  run() { return 2; }
  v = 0;
  use() { return this.run(); }
}
function value() { return 2; }
export function readIt(b: Box) { size(); run(); b.run(); return value(); }
"#;
        for (path, language) in [
            ("src/box.ts", Language::TypeScript),
            ("src/box.js", Language::JavaScript),
        ] {
            let source = if language == Language::JavaScript {
                source.replace(": number", "").replace("(b: Box)", "(b)")
            } else {
                source.to_string()
            };
            let (files, edges) = review_edges(&[(path, &source)], language);
            let targets = call_targets(&files, &edges, path, "readIt");
            for method in [("size", 2), ("value", 3)] {
                let method_uid = uid_at(&files, path, method.0, method.1);
                assert!(
                    !targets.contains(&method_uid),
                    "{path}: a bare {}() bound the accessor: {targets:?}",
                    method.0
                );
            }
            assert!(
                targets.contains(&uid_at(&files, path, "value", 8)),
                "{path}: the free function must win: {targets:?}"
            );
            assert!(
                targets.contains(&"unresolved:run".to_string()),
                "{path}: {targets:?}"
            );
            assert_eq!(
                call_targets(&files, &edges, path, "use"),
                vec![uid_at(&files, path, "run", 4)],
                "{path}: this.run() is unchanged"
            );
        }
    }

    // ── QA round: receiver expressions, construction, turbofish, variants ──

    fn qa_calls(
        files: &[(String, Vec<RawSymbol>, Vec<RawReference>)],
        edges: &[ResolvedEdge],
        from: (&str, &str),
        to: (&str, &str, u32),
    ) -> bool {
        let source = uid(files, from.0, from.1);
        let target = symbol_uid("repo:test:abc", to.0, to.1, to.2);
        edges.iter().any(|edge| {
            edge.edge_type == EdgeType::Calls
                && edge.source_uid == source
                && edge.target_uid == target
        })
    }

    #[test]
    fn qa_ts_method_on_new_static_and_chained_call_receivers_resolves() {
        let path = "a.ts";
        let source = "export class K {\n  m1(): K { return this; }\n  m2(): number { return 1; }\n  static make(): K { return new K(); }\n}\nexport function t1() { new K().m2(); }\nexport function t2() { const k = new K(); k.m1().m2(); }\nexport function t3() { K.make().m2(); }\nexport function t4() { const k: K = K.make(); k.m2(); }\n";
        let (files, edges) = review_edges(&[(path, source)], Language::TypeScript);
        for caller in ["t1", "t2", "t3", "t4"] {
            assert!(
                qa_calls(&files, &edges, (path, caller), (path, "m2", 3)),
                "{caller} must call K.m2: {edges:#?}"
            );
        }
    }

    #[test]
    fn qa_ts_expression_receiver_of_an_external_type_links_nothing() {
        let path = "a.ts";
        let source = "export class Store {\n  get(k: string): number { return 1; }\n}\nexport function user() { return new Map().get('x'); }\nexport function other() { return fetch('u').get('x'); }\n";
        let (files, edges) = review_edges(&[(path, source)], Language::TypeScript);
        for caller in ["user", "other"] {
            assert!(
                !qa_calls(&files, &edges, (path, caller), (path, "get", 2)),
                "{caller}: a Map/fetch result is not a Store: {edges:#?}"
            );
        }
    }

    #[test]
    fn qa_unique_method_fallback_refuses_a_repo_function_returning_anything() {
        let path = "a.ts";
        let source = "export class Store {\n  get(k: string): number { return 1; }\n}\nfunction q() { return new Map(); }\nexport function user() { return q().get('x'); }\n";
        let (files, edges) = review_edges(&[(path, source)], Language::TypeScript);
        assert!(
            !qa_calls(&files, &edges, (path, "user"), (path, "get", 2)),
            "an unannotated repo function may return an external Map: {edges:#?}"
        );
    }

    #[test]
    fn qa_unique_method_fallback_needs_the_head_type_to_own_the_method() {
        let path = "pkg/core.py";
        let source = "class Other:\n    def run(self):\n        return 1\n\nclass Child:\n    @staticmethod\n    def make():\n        return Child()\n\ndef user():\n    return Child.make().run()\n";
        let (files, edges) = review_edges(&[(path, source)], Language::Python);
        assert!(
            !qa_calls(&files, &edges, (path, "user"), (path, "run", 2)),
            "`run` belongs to an unrelated class: {edges:#?}"
        );
    }

    #[test]
    fn qa_python_method_on_unannotated_factory_uses_unique_method_name() {
        let path = "pkg/core.py";
        let source = "class Base:\n    def run(self):\n        return 1\n\nclass Child(Base):\n    @staticmethod\n    def make():\n        return Child()\n\ndef user():\n    return Child.make().run()\n";
        let (files, edges) = review_edges(&[(path, source)], Language::Python);
        assert!(
            qa_calls(&files, &edges, (path, "user"), (path, "run", 2)),
            "{edges:#?}"
        );
        assert!(
            edges.iter().any(|edge| edge
                .evidence
                .iter()
                .any(|evidence| evidence.kind == "unique_method_name")),
            "the fallback must say what it is: {edges:#?}"
        );
    }

    #[test]
    fn unique_method_fallback_refuses_java_library_return_types() {
        let path = "src/A.java";
        let source = "import java.util.List;\nimport java.util.Optional;\nclass A {\n    static Optional<A> find() { return Optional.of(new A()); }\n    static List<A> all() { return List.of(new A()); }\n    static List<String> names() { return List.of(); }\n    A get() { return this; }\n    boolean isEmpty() { return false; }\n    int size() { return 0; }\n}\nclass U {\n    void use() {\n        A.find().get();\n        A.all().isEmpty();\n        A.names().size();\n    }\n}\n";
        let (files, edges) = review_edges(&[(path, source)], Language::Java);
        for (method, line) in [("get", 7), ("isEmpty", 8), ("size", 9)] {
            assert!(
                !qa_calls(&files, &edges, (path, "use"), (path, method, line)),
                "`{method}` is called on an Optional/List, not on an A: {edges:#?}"
            );
        }
    }

    #[test]
    fn unique_method_fallback_refuses_ts_promise_and_array_returns() {
        let path = "a.ts";
        let source = "export class Q {\n  static all(): Promise<Q[]> { return Promise.resolve([new Q()]); }\n  static list(): Q[] { return [new Q()]; }\n  static rows() {\n    return [new Q()];\n  }\n  then(): number { return 1; }\n  each(): number { return 2; }\n  map(): number { return 3; }\n}\nexport function user() {\n  Q.all().then();\n  Q.list().each();\n  Q.rows().map();\n}\n";
        let (files, edges) = review_edges(&[(path, source)], Language::TypeScript);
        for (method, line) in [("then", 7), ("each", 8), ("map", 9)] {
            assert!(
                !qa_calls(&files, &edges, (path, "user"), (path, method, line)),
                "`{method}` is called on a Promise/array, not on a Q: {edges:#?}"
            );
        }
    }

    #[test]
    fn unique_method_fallback_follows_an_unannotated_ts_factory_returning_new() {
        let path = "a.ts";
        let source = "export class K {\n  static make() {\n    return new K();\n  }\n  each(): number { return 2; }\n}\nexport function user() { K.make().each(); }\n";
        let (files, edges) = review_edges(&[(path, source)], Language::TypeScript);
        assert!(
            qa_calls(&files, &edges, (path, "user"), (path, "each", 5)),
            "`make` returns `new K()`: {edges:#?}"
        );
    }

    #[test]
    fn unique_method_fallback_refuses_python_factories_not_returning_the_type() {
        let path = "pkg/engine.py";
        let source = "import json\n\nclass Engine:\n    def items(self):\n        return []\n\n    @classmethod\n    def from_json(cls, s):\n        return json.loads(s)\n\n    @classmethod\n    def from_dict(cls, d) -> dict:\n        return cls(d)\n\n    @staticmethod\n    def wrapped():\n        return [Engine()]\n\ndef load(s):\n    return Engine.from_json(s).items()\n\ndef load_dict(s):\n    return Engine.from_dict(s).items()\n\ndef load_wrapped():\n    return Engine.wrapped().items()\n";
        let (files, edges) = review_edges(&[(path, source)], Language::Python);
        for caller in ["load", "load_dict", "load_wrapped"] {
            assert!(
                !qa_calls(&files, &edges, (path, caller), (path, "items", 4)),
                "{caller}: the factory does not return an Engine: {edges:#?}"
            );
        }
    }

    #[test]
    fn unique_method_fallback_follows_a_python_classmethod_returning_cls() {
        let path = "pkg/engine.py";
        let source = "import json\n\nclass Engine:\n    def items(self):\n        return []\n\n    @classmethod\n    def from_json(cls, s):\n        return cls(json.loads(s))\n\ndef load(s):\n    return Engine.from_json(s).items()\n";
        let (files, edges) = review_edges(&[(path, source)], Language::Python);
        assert!(
            qa_calls(&files, &edges, (path, "load"), (path, "items", 4)),
            "`from_json` returns `cls(..)`, an Engine: {edges:#?}"
        );
    }

    #[test]
    fn returns_construction_of_needs_the_call_to_be_the_returned_value() {
        assert!(returns_construction_of("return Child()", "Child"));
        assert!(returns_construction_of("return new K();", "K"));
        assert!(returns_construction_of("return cls(json.loads(s))", "cls"));
        assert!(!returns_construction_of("return [Child()]", "Child"));
        assert!(!returns_construction_of("[new Q()]", "Q"));
        assert!(!returns_construction_of(
            "return Child().configure()",
            "Child"
        ));
        assert!(!returns_construction_of("return Children()", "Child"));
        assert!(!returns_construction_of("returned = Child()", "Child"));
        assert!(!returns_construction_of(
            "return Child() if x else None",
            "Child"
        ));
    }

    #[test]
    fn qa_python_unique_method_fallback_refuses_a_name_declared_twice() {
        let path = "pkg/core.py";
        let source = "class A:\n    def get(self):\n        return 1\n\nclass B:\n    def get(self):\n        return 2\n    @staticmethod\n    def make():\n        return B()\n\ndef user():\n    return B.make().get()\n";
        let (files, edges) = review_edges(&[(path, source)], Language::Python);
        // `user` does call `B.make` (line 9); neither `get` may be guessed.
        for get_line in [2, 6] {
            assert!(
                !qa_calls(&files, &edges, (path, "user"), (path, "get", get_line)),
                "`get` is declared twice; nothing may be guessed: {edges:#?}"
            );
        }
    }

    #[test]
    fn qa_rust_trait_default_method_resolves_through_the_impl() {
        let path = "src/lib.rs";
        let source = "pub trait Speak {\n    fn speak(&self) -> String;\n    fn shout(&self) -> String { self.speak() }\n}\npub struct Dog;\nimpl Speak for Dog {\n    fn speak(&self) -> String { String::new() }\n}\nimpl Dog {\n    pub fn new() -> Self { Dog }\n}\npub fn entry() {\n    let d = Dog::new();\n    let _ = d.shout();\n}\n";
        let (files, edges) = review_edges(&[(path, source)], Language::Rust);
        assert!(
            qa_calls(&files, &edges, (path, "entry"), (path, "shout", 3)),
            "{edges:#?}"
        );
    }

    #[test]
    fn qa_rust_method_on_constructor_call_receiver_resolves() {
        let path = "src/lib.rs";
        let source = "pub struct Dog;\nimpl Dog {\n    pub fn new() -> Self { Dog }\n    pub fn bark(&self) {}\n}\npub fn entry() {\n    Dog::new().bark();\n}\n";
        let (files, edges) = review_edges(&[(path, source)], Language::Rust);
        assert!(
            qa_calls(&files, &edges, (path, "entry"), (path, "bark", 4)),
            "{edges:#?}"
        );
    }

    #[test]
    fn go_signature_parameters_do_not_hide_real_helpers_and_callable_parameters_still_shadow() {
        let path = "app.go";
        let source = "package app\ntype API interface { Read(helper *Store) }\ntype Callback func(helper *Store)\ntype Store struct{}\nfunc helper() {}\nfunc (s *Store) Get() {}\nfunc entry() {\n type Local func(helper *Store)\n helper()\n}\nfunc param(helper func()) { helper() }\nfunc literal() { callback := func(helper func()) { helper() }; _ = callback }\nfunc typed(argument *Store) { argument.Get() }\nfunc (receiver *Store) member() { receiver.Get() }\nfunc typedLiteral() { callback := func(argument *Store) { argument.Get() }; _ = callback }\n";
        let (files, edges) = review_edges(&[(path, source)], Language::Go);
        assert!(
            qa_calls(&files, &edges, (path, "entry"), (path, "helper", 5)),
            "signature parameter names cannot hide the package helper: {edges:#?}"
        );
        for caller in ["param", "literal"] {
            assert!(
                !qa_calls(&files, &edges, (path, caller), (path, "helper", 5)),
                "actual callable parameter must still shadow: {caller}: {edges:#?}"
            );
        }
        for caller in ["typed", "member", "typedLiteral"] {
            assert!(
                qa_calls(&files, &edges, (path, caller), (path, "Get", 6)),
                "actual function/method/literal parameter must retain type evidence: {caller}: {edges:#?}"
            );
        }
    }

    #[test]
    fn go_statement_initializers_restore_package_imports_without_losing_inner_shadows() {
        let app = r#"package app
import helper "lib"
type Local struct{}
func (value Local) Ping() {}
func localValue() Local { return Local{} }
func Control() { helper.Ping() }
func AfterIf() { if helper := 1; helper > 0 { _ = helper }; helper.Ping() }
func AfterElseIf() {
 if helper := 1; helper < 0 { _ = helper } else if helper := 2; helper > 0 { _ = helper }
 helper.Ping()
}
func AfterSwitch() { switch helper := 1; helper { case 1: _ = helper }; helper.Ping() }
func AfterTypeSwitch(value interface{}) { switch helper := 1; value.(type) { case int: _ = helper }; helper.Ping() }
func AfterNestedIf() { if true { if helper := 1; helper > 0 { _ = helper }; helper.Ping() } }
func InsideIf() { if helper := localValue(); true { helper.Ping() } }
func InsideElse() { if helper := localValue(); false { _ = helper } else { helper.Ping() } }
func InsideElseIf() { if helper := localValue(); false { _ = helper } else if true { helper.Ping() } }
func InsideSwitch() { switch helper := localValue(); { default: helper.Ping() } }
func InsideTypeSwitch(value interface{}) { switch helper := localValue(); value.(type) { case int: helper.Ping() } }
"#;
        let (files, edges) = review_edges(
            &[
                ("lib/lib.go", "package lib\nfunc Ping() {}\n"),
                ("app.go", app),
            ],
            Language::Go,
        );
        // Each caller has only one Ping use: a post-statement success cannot
        // conceal a mistaken import edge from an inner shadowed call.
        assert_eq!(
            review_callers(&files, &edges, "lib/lib.go", "Ping"),
            [
                "Control",
                "AfterIf",
                "AfterElseIf",
                "AfterSwitch",
                "AfterTypeSwitch",
                "AfterNestedIf",
            ]
            .into_iter()
            .map(|caller| uid(&files, "app.go", caller))
            .collect(),
            "only calls outside the initializer's lifetime may use the import: {edges:#?}"
        );
        assert_eq!(
            review_callers(&files, &edges, "app.go", "Ping"),
            [
                "InsideIf",
                "InsideElse",
                "InsideElseIf",
                "InsideSwitch",
                "InsideTypeSwitch",
            ]
            .into_iter()
            .map(|caller| uid(&files, "app.go", caller))
            .collect(),
            "initializer values still shadow throughout their statement: {edges:#?}"
        );
    }

    #[test]
    fn go_case_declarations_do_not_shadow_package_imports_in_sibling_or_later_code() {
        let app = r#"package app
import helper "lib"
type Local struct{}
func (value Local) Ping() {}
func localValue() Local { return Local{} }
func AfterCase(value int) { switch value { case 1: helper := 1; _ = helper }; helper.Ping() }
func SiblingCase(value int) { switch value { case 1: helper := 1; _ = helper; case 2: helper.Ping() } }
func SiblingDefault(value int) { switch value { case 1: helper := 1; _ = helper; default: helper.Ping() } }
func AfterDefault(value int) { switch value { default: helper := 1; _ = helper }; helper.Ping() }
func SiblingTypeCase(value interface{}) { switch value.(type) { case int: helper := 1; _ = helper; case string: helper.Ping() } }
func AfterTypeDefault(value interface{}) { switch value.(type) { default: helper := 1; _ = helper }; helper.Ping() }
func SiblingCommunication(ch <-chan int) { select { case <-ch: helper := 1; _ = helper; default: helper.Ping() } }
func AfterCommunication(ch <-chan int) { select { case <-ch: helper := 1; _ = helper; default: }; helper.Ping() }
func InsideCase(value int) { switch value { case 1: helper := localValue(); helper.Ping() } }
func InsideDefault(value int) { switch value { default: helper := localValue(); helper.Ping() } }
func InsideTypeCase(value interface{}) { switch value.(type) { case int: helper := localValue(); helper.Ping() } }
func InsideCommunication(ch <-chan int) { select { case <-ch: helper := localValue(); helper.Ping(); default: } }
"#;
        let (files, edges) = review_edges(
            &[
                ("lib/lib.go", "package lib\nfunc Ping() {}\n"),
                ("app.go", app),
            ],
            Language::Go,
        );
        assert_eq!(
            review_callers(&files, &edges, "lib/lib.go", "Ping"),
            [
                "AfterCase",
                "SiblingCase",
                "SiblingDefault",
                "AfterDefault",
                "SiblingTypeCase",
                "AfterTypeDefault",
                "SiblingCommunication",
                "AfterCommunication",
            ]
            .into_iter()
            .map(|caller| uid(&files, "app.go", caller))
            .collect(),
            "case-local declarations must not escape their implicit block: {edges:#?}"
        );
        assert_eq!(
            review_callers(&files, &edges, "app.go", "Ping"),
            [
                "InsideCase",
                "InsideDefault",
                "InsideTypeCase",
                "InsideCommunication",
            ]
            .into_iter()
            .map(|caller| uid(&files, "app.go", caller))
            .collect(),
            "case-local typed values must still shadow the imported package: {edges:#?}"
        );
    }

    #[test]
    fn go_statement_scopes_restore_outer_call_and_slice_receiver_origins() {
        let app = r#"package app
import outer "outer"
import inner "inner"
func AfterIf() { client := outer.New(); if client := inner.New(); true { _ = client }; client.Ping() }
func AfterSwitch() { client := outer.New(); switch client := inner.New(); { default: _ = client }; client.Ping() }
func AfterTypeSwitch(value interface{}) { client := outer.New(); switch client := inner.New(); value.(type) { case int: _ = client }; client.Ping() }
func AfterCase(value int) { client := outer.New(); switch value { case 1: client := inner.New(); _ = client }; client.Ping() }
func SiblingCase(value int) { client := outer.New(); switch value { case 1: client := inner.New(); _ = client; case 2: client.Ping() } }
func AfterSlice() { clients := []outer.Client{{}}; if clients := []inner.Client{{}}; true { _ = clients }; for _, client := range clients { client.Ping() } }
func InitializerRHS() { if outer := outer.New(); true { outer.Ping() } }
func InsideIf() { if client := inner.New(); true { client.Ping() } }
func InsideElse() { if client := inner.New(); false { _ = client } else { client.Ping() } }
func InsideCase(value int) { switch value { case 1: client := inner.New(); client.Ping() } }
"#;
        let (files, edges) = review_edges(
            &[
                (
                    "outer/outer.go",
                    "package outer\ntype Client struct{}\nfunc New() Client { return Client{} }\nfunc (value Client) Ping() {}\n",
                ),
                (
                    "inner/inner.go",
                    "package inner\ntype Client struct{}\nfunc New() Client { return Client{} }\nfunc (value Client) Ping() {}\n",
                ),
                ("app.go", app),
            ],
            Language::Go,
        );
        assert_eq!(
            review_callers(&files, &edges, "outer/outer.go", "Ping"),
            [
                "AfterIf",
                "AfterSwitch",
                "AfterTypeSwitch",
                "AfterCase",
                "SiblingCase",
                "AfterSlice",
                "InitializerRHS",
            ]
            .into_iter()
            .map(|caller| uid(&files, "app.go", caller))
            .collect(),
            "factory and literal evidence must expire with the same scope as shadow bindings: {edges:#?}"
        );
        assert_eq!(
            review_callers(&files, &edges, "inner/inner.go", "Ping"),
            ["InsideIf", "InsideElse", "InsideCase"]
                .into_iter()
                .map(|caller| uid(&files, "app.go", caller))
                .collect(),
            "identically named types cannot donate the inner receiver outside its scope: {edges:#?}"
        );
        assert!(
            qa_calls(
                &files,
                &edges,
                ("app.go", "InitializerRHS"),
                ("outer/outer.go", "New", 3)
            ),
            "an initializer's RHS still sees the imported package before its own binding: {edges:#?}"
        );
    }

    #[test]
    fn go_range_residual_forms_keep_exact_owners_in_fresh_and_incremental_resolution() {
        let app = "package app\nimport alias \"store\"\nfunc initialized() { var items = []alias.Store{{}}; for _, item := range items { item.Get() } }\nfunc identifier(items []alias.Store) { for _, item := range (( /* source */ items )) { item.Get() } }\nfunc literal() { for _, item := range ([]alias.Store{{}}) { item.Get() } }\nfunc factory() { for _, item := range (alias.All()) { item.Get() } }\nfunc named() { for _, item := range alias.Named() { item.Get() } }\nfunc pointer() { for _, item := range alias.NamedPointer() { item.Get() } }\nfunc assignedVarFactory() { var items = alias.Named(); for _, item := range items { item.Get() } }\nfunc shadowSource(items []alias.Store) { for _, items := range (items) { items.Get() } }\n";
        let input = [
            (
                "store/types.go",
                "package store\ntype Store struct{}\nfunc All() []Store { return nil }\nfunc Named() (items []Store) { return nil }\nfunc NamedPointer() (items []*Store) { return nil }\n",
            ),
            (
                "store/methods.go",
                "package store\nfunc (recv Store) Get() {}\n",
            ),
            (
                "other/types.go",
                "package other\ntype Store struct{}\nfunc (recv Store) Get() {}\nfunc Named() (items []Store) { return nil }\n",
            ),
            ("app.go", app),
        ];
        let (files, fresh) = review_edges(&input, Language::Go);
        let parsed = parse_source(Path::new("app.go"), app).unwrap();
        let envs = std::collections::HashMap::from([(
            "app.go".into(),
            crate::types::TypeEnvironment::build(
                app,
                Language::Go,
                &parsed.symbols,
                &parsed.type_bindings,
            ),
        )]);
        let mut incremental = files.clone();
        for (path, _, references) in &mut incremental {
            if path != "app.go" {
                references.clear();
            }
        }
        let partial = resolve_references_with_context(
            &incremental,
            Language::Go,
            "repo:test:abc",
            &WorkspaceContext::default(),
            Some(&envs),
            None,
        );
        for (label, edges) in [
            ("fresh", &fresh),
            ("incremental exact-callee fallback", &partial),
        ] {
            for caller in [
                "initialized",
                "identifier",
                "literal",
                "factory",
                "named",
                "pointer",
                "assignedVarFactory",
                "shadowSource",
            ] {
                assert!(
                    qa_calls(
                        &files,
                        edges,
                        ("app.go", caller),
                        ("store/methods.go", "Get", 2)
                    ),
                    "{label} {caller}: {edges:#?}"
                );
                assert!(
                    !qa_calls(
                        &files,
                        edges,
                        ("app.go", caller),
                        ("other/types.go", "Get", 3)
                    ),
                    "foreign owner {label} {caller}: {edges:#?}"
                );
            }
        }
    }

    #[test]
    fn go_qualified_named_factory_keeps_unchanged_callee_import_origin_without_type_environment() {
        let app = "package app\nimport maker \"factory\"\nfunc consume() { for _, item := range maker.Named() { item.Get() } }\n";
        let input = [
            ("store/types.go", "package store\ntype Store struct{}\n"),
            (
                "store/methods.go",
                "package store\nfunc (recv Store) Get() {}\n",
            ),
            (
                "other/types.go",
                "package other\ntype Store struct{}\nfunc (recv Store) Get() {}\n",
            ),
            (
                "factory/all.go",
                "package factory\nimport choice \"store\"\nfunc Named() (items []choice.Store) { return nil }\n",
            ),
            ("app.go", app),
        ];
        let (files, fresh) = review_edges(&input, Language::Go);
        let parsed = parse_source(Path::new("app.go"), app).unwrap();
        let envs = std::collections::HashMap::from([(
            "app.go".into(),
            crate::types::TypeEnvironment::build(
                app,
                Language::Go,
                &parsed.symbols,
                &parsed.type_bindings,
            ),
        )]);
        let mut incremental = files.clone();
        for (path, _, references) in &mut incremental {
            if path != "app.go" {
                // The unchanged callee has no rebuilt type environment or body
                // references. Its existing import/package facts are still the
                // authority needed to interpret its declared alias `choice`.
                references.retain(|reference| {
                    matches!(
                        reference.kind,
                        ReferenceKind::Import
                            | ReferenceKind::ImportAlias
                            | ReferenceKind::PackageBinding
                    )
                });
            }
        }
        let partial = resolve_references_with_context(
            &incremental,
            Language::Go,
            "repo:test:abc",
            &WorkspaceContext::default(),
            Some(&envs),
            None,
        );
        for (label, edges) in [("fresh", &fresh), ("unchanged qualified callee", &partial)] {
            assert!(
                qa_calls(
                    &files,
                    edges,
                    ("app.go", "consume"),
                    ("store/methods.go", "Get", 2)
                ),
                "{label}: {edges:#?}"
            );
            assert!(
                !qa_calls(
                    &files,
                    edges,
                    ("app.go", "consume"),
                    ("other/types.go", "Get", 3)
                ),
                "foreign owner {label}: {edges:#?}"
            );
        }
    }

    #[test]
    fn go_var_slice_evidence_preserves_later_name_shadow_guards_and_annotations() {
        let app = "package app\nimport a \"store\"\nimport b \"other\"\nfunc afterShadow() { var items = []a.Store{}; { var items = []b.Store{}; for _, item := range (items) { item.Get() } } }\nfunc beforeShadow(items []a.Store) { { for _, item := range (items) { item.Get() }; var items = []b.Store{}; _ = items } }\nfunc multiShadow() { var items = []a.Store{}; { var unused, items = 0, []interface{ Get() }{}; _ = unused; for _, item := range (items) { item.Get() } } }\nfunc groupedClosure() { var items = []a.Store{}; _ = items; _ = func(unused, items []b.Store) { _ = unused; for _, item := range (items) { item.Get() } } }\nfunc explicitType() { var items []b.Store = nil; for _, item := range (items) { item.Get() } }\nfunc unknown(items []interface{ Get() }) { for _, item := range (items) { item.Get() } }\nfunc interfaceFactory() { for _, item := range a.Unknown() { item.Get() } }\nfunc multiResult() { items, _ := a.Multi(); for _, item := range items { item.Get() } }\nfunc groupedResult() { items, _ := a.Grouped(); for _, item := range items { item.Get() } }\nfunc afterTypedConst() { var items = []a.Store{}; for _, item := range items { _ = item; { const item b.Scalar = 0; item.Get() } } }\nfunc beforeTypedConst() { var items = []a.Store{}; for _, item := range items { { item.Get(); const item b.Scalar = 0; _ = item } } }\nfunc afterUntypedConst() { var items = []a.Store{}; for _, item := range items { _ = item; { const item = b.Scalar(0); item.Get() } } }\nfunc beforeUntypedConst() { var items = []a.Store{}; for _, item := range items { { item.Get(); const item = b.Scalar(0); _ = item } } }\nfunc afterGroupedConst() { var items = []a.Store{}; for _, item := range items { _ = item; { const unused, item b.Scalar = 0, 0; _ = unused; item.Get() } } }\nfunc beforeGroupedConst() { var items = []a.Store{}; for _, item := range items { { item.Get(); const unused, item b.Scalar = 0, 0; _ = unused; _ = item } } }\nfunc afterInheritedConst() { var items = []a.Store{}; for _, item := range items { _ = item; { const (unused = b.Scalar(0); item); _ = unused; item.Get() } } }\nfunc beforeInheritedConst() { var items = []a.Store{}; for _, item := range items { { item.Get(); const (unused = b.Scalar(0); item); _ = unused; _ = item } } }\n";
        let input = [
            (
                "store/types.go",
                "package store\ntype Store struct{}\nfunc (recv Store) Get() {}\nfunc Unknown() (items []interface{ Get() }) { return nil }\nfunc Multi() (items []Store, err error) { return nil, nil }\nfunc Grouped() (left, right []Store) { return nil, nil }\n",
            ),
            (
                "other/types.go",
                "package other\ntype Store struct{}\nfunc (recv Store) Get() {}\ntype Scalar int\nfunc (recv Scalar) Get() {}\n",
            ),
            ("app.go", app),
        ];
        let (files, edges) = review_edges(&input, Language::Go);
        let other_symbols = &files
            .iter()
            .find(|(path, _, _)| path == "other/types.go")
            .unwrap()
            .1;
        assert!(
            other_symbols
                .iter()
                .any(|symbol| symbol.name == "Store" && symbol.kind == SymbolKind::Class)
        );
        assert!(
            other_symbols
                .iter()
                .any(|symbol| symbol.name == "Scalar" && symbol.kind == SymbolKind::TypeAlias)
        );
        // Scalar constants are valid Go, but scalar-alias receiver resolution
        // is deliberately outside this inference fix. Its real Get symbol
        // makes the conservative refusal checks non-vacuous.
        let scalar_get = uid_at(&files, "other/types.go", "Get", 5);
        assert_ne!(scalar_get, uid_at(&files, "other/types.go", "Get", 3));
        for (caller, expected, other) in [
            ("afterShadow", "other/types.go", "store/types.go"),
            ("beforeShadow", "store/types.go", "other/types.go"),
            ("explicitType", "other/types.go", "store/types.go"),
            ("groupedClosure", "other/types.go", "store/types.go"),
            ("beforeTypedConst", "store/types.go", "other/types.go"),
            ("beforeUntypedConst", "store/types.go", "other/types.go"),
            ("beforeGroupedConst", "store/types.go", "other/types.go"),
            ("beforeInheritedConst", "store/types.go", "other/types.go"),
        ] {
            assert!(
                qa_calls(&files, &edges, ("app.go", caller), (expected, "Get", 3)),
                "{caller}: {edges:#?}"
            );
            assert!(
                !qa_calls(&files, &edges, ("app.go", caller), (other, "Get", 3)),
                "wrong owner in {caller}: {edges:#?}"
            );
            assert!(
                !qa_calls(
                    &files,
                    &edges,
                    ("app.go", caller),
                    ("other/types.go", "Get", 5)
                ),
                "scalar donor in {caller}: {edges:#?}"
            );
        }
        for caller in [
            "multiShadow",
            "unknown",
            "interfaceFactory",
            "multiResult",
            "groupedResult",
            "afterTypedConst",
            "afterUntypedConst",
            "afterGroupedConst",
            "afterInheritedConst",
        ] {
            for (target, line) in [
                ("store/types.go", 3),
                ("other/types.go", 3),
                ("other/types.go", 5),
            ] {
                assert!(
                    !qa_calls(&files, &edges, ("app.go", caller), (target, "Get", line)),
                    "concrete donor {caller}: {edges:#?}"
                );
            }
        }
    }

    #[test]
    fn go_slice_ranges_resolve_without_receiver_or_parameter_type_donors() {
        for (receiver, argument, element) in [("c", "c", "c"), ("recv", "directArg", "element")] {
            let source = format!(
                "package app\ntype Local struct{{}}\nfunc ({receiver} Local) Deliver() {{}}\n\
                 func direct({argument} Local) {{ {argument}.Deliver() }}\n\
                 func values(items []Local) {{ for _, {element} := range items {{ {element}.Deliver() }} }}\n\
                 func pointers(items []*Local) {{ for _, {element} := range items {{ {element}.Deliver() }} }}\n\
                 func literal() {{ items := []Local{{{{}}}}; for _, {element} := range items {{ {element}.Deliver() }} }}\n\
                 func pointerLiteral() {{ items := []*Local{{{{}}}}; for _, {element} := range items {{ {element}.Deliver() }} }}\n\
                 func inlineLiteral() {{ for _, {element} := range []Local{{{{}}}} {{ {element}.Deliver() }} }}\n\
                 func makeSlice() []Local {{ return []Local{{{{}}}} }}\n\
                 func factory() {{ for _, {element} := range makeSlice() {{ {element}.Deliver() }} }}\n\
                 func assignedFactory() {{ items := makeSlice(); for _, {element} := range items {{ {element}.Deliver() }} }}\n"
            );
            let (files, edges) = review_edges(&[("app.go", &source)], Language::Go);
            for caller in [
                "direct",
                "values",
                "pointers",
                "literal",
                "pointerLiteral",
                "inlineLiteral",
                "factory",
                "assignedFactory",
            ] {
                assert!(
                    qa_calls(&files, &edges, ("app.go", caller), ("app.go", "Deliver", 3)),
                    "{caller}, receiver={receiver}, argument={argument}, element={element}: {edges:#?}"
                );
            }
        }
    }

    #[test]
    fn go_slice_ranges_preserve_qualified_and_exact_factory_return_owners() {
        let app = "package app\nimport alias \"store\"\nfunc parameter(items []*alias.Store) { for _, item := range items { item.Get() } }\nfunc literal() { items := []alias.Store{{}}; for _, item := range items { item.Get() } }\nfunc factory() { for _, item := range alias.All() { item.Get() } }\nfunc assigned() { items := alias.All(); for _, item := range items { item.Get() } }\n";
        let input = [
            (
                "store/types.go",
                "package store\ntype Store struct{}\nfunc All() []*Store { return []*Store{{}} }\n",
            ),
            (
                "store/methods.go",
                "package store\nfunc (recv *Store) Get() {}\n",
            ),
            (
                "other/types.go",
                "package other\ntype Store struct{}\nfunc (recv *Store) Get() {}\nfunc All() []*Store { return []*Store{{}} }\n",
            ),
            ("app.go", app),
        ];
        let (files, edges) = review_edges(&input, Language::Go);
        for caller in ["parameter", "literal", "factory", "assigned"] {
            assert!(
                qa_calls(
                    &files,
                    &edges,
                    ("app.go", caller),
                    ("store/methods.go", "Get", 2)
                ),
                "{caller}: {edges:#?}"
            );
            assert!(
                !qa_calls(
                    &files,
                    &edges,
                    ("app.go", caller),
                    ("other/types.go", "Get", 3)
                ),
                "foreign homonym in {caller}: {edges:#?}"
            );
        }
        // Incremental resolution may retain the callee declaration without its
        // references or a rebuilt type environment. Its exact return still owns
        // the slice element type; another same-named factory cannot donate it.
        let mut envs = std::collections::HashMap::new();
        let parsed = parse_source(Path::new("app.go"), app).unwrap();
        envs.insert(
            "app.go".into(),
            crate::types::TypeEnvironment::build(
                app,
                Language::Go,
                &parsed.symbols,
                &parsed.type_bindings,
            ),
        );
        let mut incremental = files.clone();
        for (file, _, references) in &mut incremental {
            if file != "app.go" {
                references.clear();
            }
        }
        let edges = resolve_references_with_context(
            &incremental,
            Language::Go,
            "repo:test:abc",
            &WorkspaceContext::default(),
            Some(&envs),
            None,
        );
        for caller in ["parameter", "literal", "factory", "assigned"] {
            assert!(
                qa_calls(
                    &incremental,
                    &edges,
                    ("app.go", caller),
                    ("store/methods.go", "Get", 2)
                ),
                "incremental {caller}: {edges:#?}"
            );
            assert!(
                !qa_calls(
                    &incremental,
                    &edges,
                    ("app.go", caller),
                    ("other/types.go", "Get", 3)
                ),
                "incremental foreign {caller}: {edges:#?}"
            );
        }
    }

    #[test]
    fn go_incremental_method_owners_keep_package_build_and_language_boundaries() {
        // Wrong-package/ignored receiver declarations are parser negatives:
        // they must not gain an edge merely by spelling the same parent name.
        let mut files = parsed_files(&[
            ("store/types.go", "package store\ntype Store struct{}\n"),
            (
                "store/methods.go",
                "package store\nfunc (recv Store) Get() {}\n",
            ),
            (
                "store/foreign.go",
                "package other\nfunc (recv Store) Get() {}\n",
            ),
            (
                "store/ignored.go",
                "//go:build ignore\n\npackage store\nfunc (recv Store) Get() {}\n",
            ),
            (
                "store/methods_test.go",
                "package store_test\nfunc (recv Store) Get() {}\n",
            ),
            (
                "store/Foreign.java",
                "package store; class Store { public void Get() {} }\n",
            ),
        ]);
        for (file, _, references) in &mut files {
            if file == "store/methods.go" || file == "store/methods_test.go" {
                references.clear();
            }
        }
        let languages = files
            .iter()
            .map(|(file, _, _)| {
                (
                    file.clone(),
                    nestweaver_parser::detect_language(Path::new(file)).unwrap(),
                )
            })
            .collect::<std::collections::HashMap<_, _>>();
        let graph = build_import_graph_with_languages(
            &files,
            Language::Go,
            Some(&languages),
            &WorkspaceContext::default(),
        );
        assert!(graph.go_package_of("store/methods.go").is_none());
        assert!(graph.go_package_of("store/methods_test.go").is_none());
        assert_eq!(graph.go_package_of("store/foreign.go"), Some("other"));
        assert!(graph.go_build_ignored("store/ignored.go"));
        let mut symbol_map = std::collections::HashMap::<String, Vec<(&str, &RawSymbol)>>::new();
        for (file, symbols, _) in &files {
            for symbol in symbols {
                symbol_map
                    .entry(symbol.name.clone())
                    .or_default()
                    .push((file, symbol));
            }
        }
        let origin = TypedReceiverOrigin {
            file: "store/types.go".into(),
            name: "Store".into(),
            rust_class_position: None,
        };
        let mut checked = 0;
        for (file, symbols, _) in &files {
            let Some(method) = symbols.iter().find(|symbol| symbol.name == "Get") else {
                continue;
            };
            checked += 1;
            assert_eq!(
                method_belongs_to_origin(file, method, &origin, &graph, &symbol_map, None),
                file == "store/methods.go",
                "owner boundary: {file}"
            );
            // A non-Go exact origin cannot use the Go same-directory fallback.
            let non_go_origin = TypedReceiverOrigin {
                file: "store/Foreign.java".into(),
                name: "Store".into(),
                rust_class_position: None,
            };
            if file == "store/methods.go" {
                assert!(!method_belongs_to_origin(
                    file,
                    method,
                    &non_go_origin,
                    &graph,
                    &symbol_map,
                    None
                ));
            }
        }
        assert_eq!(
            checked, 5,
            "all positive and negative method fixtures must be exercised"
        );
    }

    #[test]
    fn go_unknown_package_method_owner_requires_a_unique_named_go_type() {
        for (declaration, kind) in [
            ("type Store int", SymbolKind::TypeAlias),
            ("type Store interface{}", SymbolKind::Interface),
            ("type Store struct{}", SymbolKind::Class),
        ] {
            // The named scalar and struct standalone programs are valid Go.
            // A method on an interface is an intentional parser-only negative.
            let standalone = format!(
                "//go:build ignore\n\npackage main\n{declaration}\nfunc (recv Store) Get() {{}}\nfunc main() {{}}\n"
            );
            let mut files = parsed_files(&[
                ("store/types.go", "package store\ntype Store struct{}\n"),
                (
                    "store/methods.go",
                    "package store\nfunc (recv Store) Live() {}\n",
                ),
                ("store/ignored.go", &standalone),
            ]);
            let ignored_kind = files
                .iter()
                .find(|(file, _, _)| file == "store/ignored.go")
                .unwrap()
                .1
                .iter()
                .find(|symbol| symbol.name == "Store")
                .unwrap()
                .kind;
            assert_eq!(
                ignored_kind, kind,
                "counterfixture must include the conflicting type: {declaration}"
            );
            let known_graph = build_import_graph_with_languages(
                &files,
                Language::Go,
                None,
                &WorkspaceContext::default(),
            );
            for (file, _, references) in &mut files {
                if file == "store/ignored.go" {
                    references.clear();
                }
            }
            let unknown_graph = build_import_graph_with_languages(
                &files,
                Language::Go,
                None,
                &WorkspaceContext::default(),
            );
            assert!(!unknown_graph.go_build_ignored("store/ignored.go"));
            assert!(unknown_graph.go_package_of("store/ignored.go").is_none());
            let mut symbol_map =
                std::collections::HashMap::<String, Vec<(&str, &RawSymbol)>>::new();
            for (file, symbols, _) in &files {
                for symbol in symbols {
                    symbol_map
                        .entry(symbol.name.clone())
                        .or_default()
                        .push((file, symbol));
                }
            }
            let origin = TypedReceiverOrigin {
                file: "store/types.go".into(),
                name: "Store".into(),
                rust_class_position: None,
            };
            let ignored_method = files
                .iter()
                .find(|(file, _, _)| file == "store/ignored.go")
                .unwrap()
                .1
                .iter()
                .find(|symbol| symbol.name == "Get")
                .unwrap();
            assert!(
                !method_belongs_to_origin(
                    "store/ignored.go",
                    ignored_method,
                    &origin,
                    &known_graph,
                    &symbol_map,
                    None
                ),
                "known ignored program"
            );
            assert!(
                !method_belongs_to_origin(
                    "store/ignored.go",
                    ignored_method,
                    &origin,
                    &unknown_graph,
                    &symbol_map,
                    None
                ),
                "missing ignored marker must not borrow the production type: {declaration}"
            );
            let live_method = files
                .iter()
                .find(|(file, _, _)| file == "store/methods.go")
                .unwrap()
                .1
                .iter()
                .find(|symbol| symbol.name == "Live")
                .unwrap();
            assert!(
                method_belongs_to_origin(
                    "store/methods.go",
                    live_method,
                    &origin,
                    &unknown_graph,
                    &symbol_map,
                    None
                ),
                "known matching packages retain ownership despite foreign homonym"
            );
        }
        // With no competing Go type declaration, missing ordinary package
        // clauses still permit the exact sibling method (the incremental fix).
        let mut files = parsed_files(&[
            ("store/types.go", "package store\ntype Store struct{}\n"),
            (
                "store/methods.go",
                "package store\nfunc (recv Store) Get() {}\n",
            ),
            ("store/Other.java", "class Store {}\n"),
        ]);
        for (_, _, references) in &mut files {
            references.clear();
        }
        let graph = build_import_graph_with_languages(
            &files,
            Language::Go,
            None,
            &WorkspaceContext::default(),
        );
        let mut symbol_map = std::collections::HashMap::<String, Vec<(&str, &RawSymbol)>>::new();
        for (file, symbols, _) in &files {
            for symbol in symbols {
                symbol_map
                    .entry(symbol.name.clone())
                    .or_default()
                    .push((file, symbol));
            }
        }
        let origin = TypedReceiverOrigin {
            file: "store/types.go".into(),
            name: "Store".into(),
            rust_class_position: None,
        };
        let method = files
            .iter()
            .find(|(file, _, _)| file == "store/methods.go")
            .unwrap()
            .1
            .iter()
            .find(|symbol| symbol.name == "Get")
            .unwrap();
        assert!(method_belongs_to_origin(
            "store/methods.go",
            method,
            &origin,
            &graph,
            &symbol_map,
            None
        ));
        symbol_map.remove("Store");
        assert!(
            !method_belongs_to_origin(
                "store/methods.go",
                method,
                &origin,
                &graph,
                &symbol_map,
                None
            ),
            "unknown package route must retain an actual type declaration"
        );
    }

    #[test]
    fn go_cross_file_test_method_ownership_requires_known_matching_packages() {
        let files = parsed_files(&[
            (
                "store/origin_test.go",
                "package store\ntype TestOnly struct{}\nfunc (recv TestOnly) Own() {}\n",
            ),
            (
                "store/internal_test.go",
                "package store\nfunc (recv TestOnly) Get() {}\n",
            ),
            (
                "store/external_test.go",
                "package store_test\ntype TestOnly struct{}\nfunc (recv TestOnly) Get() {}\n",
            ),
        ]);
        let graph = build_import_graph_with_languages(
            &files,
            Language::Go,
            None,
            &WorkspaceContext::default(),
        );
        let mut symbol_map = std::collections::HashMap::<String, Vec<(&str, &RawSymbol)>>::new();
        for (file, symbols, _) in &files {
            for symbol in symbols {
                symbol_map
                    .entry(symbol.name.clone())
                    .or_default()
                    .push((file, symbol));
            }
        }
        let origin = TypedReceiverOrigin {
            file: "store/origin_test.go".into(),
            name: "TestOnly".into(),
            rust_class_position: None,
        };
        for (file, symbols, _) in &files {
            let method = symbols
                .iter()
                .find(|symbol| symbol.kind == SymbolKind::Method)
                .unwrap();
            assert_eq!(
                method_belongs_to_origin(file, method, &origin, &graph, &symbol_map, None),
                file != "store/external_test.go",
                "known package: {file}"
            );
        }
        let mut unknown = files.clone();
        for (_, _, references) in &mut unknown {
            references.clear();
        }
        let unknown_graph = build_import_graph_with_languages(
            &unknown,
            Language::Go,
            None,
            &WorkspaceContext::default(),
        );
        for (file, symbols, _) in &files {
            let method = symbols
                .iter()
                .find(|symbol| symbol.kind == SymbolKind::Method)
                .unwrap();
            assert_eq!(
                method_belongs_to_origin(file, method, &origin, &unknown_graph, &symbol_map, None),
                file == "store/origin_test.go",
                "unknown test package must not invent cross-file ownership: {file}"
            );
        }
        // One retained package clause still cannot classify another test file.
        let mut partial = files.clone();
        for (file, _, references) in &mut partial {
            if file != "store/origin_test.go" {
                references.clear();
            }
        }
        let partial_graph = build_import_graph_with_languages(
            &partial,
            Language::Go,
            None,
            &WorkspaceContext::default(),
        );
        for (file, symbols, _) in &files {
            let method = symbols
                .iter()
                .find(|symbol| symbol.kind == SymbolKind::Method)
                .unwrap();
            assert_eq!(
                method_belongs_to_origin(file, method, &origin, &partial_graph, &symbol_map, None),
                file == "store/origin_test.go",
                "one known test package is insufficient: {file}"
            );
        }
    }

    #[test]
    fn go_slice_range_nested_shadows_and_indices_do_not_borrow_element_types() {
        let path = "app.go";
        let source = "package app\ntype First struct{}\nfunc (recv First) Deliver() {}\ntype Second struct{}\nfunc (recv Second) Deliver() {}\nfunc nested(outer []First, inner []Second) {\n for _, element := range outer {\n  element.Deliver()\n  for _, element := range inner { element.Deliver() }\n  element.Deliver()\n }\n}\nfunc shadow(items []First) {\n { items := []Second{{}}; for _, element := range items { element.Deliver() } }\n}\n// Parser negatives below deliberately call a method on an integer or interface.\nfunc index(items []First) { for element := range items { element.Deliver() } }\nfunc namedIndex(items []First) { for element, _ := range items { element.Deliver() } }\nfunc unknown(items []interface{}) { for _, element := range items { element.Deliver() } }\n";
        let parsed = parse_source(Path::new(path), source).unwrap();
        let env = crate::types::TypeEnvironment::build(
            source,
            Language::Go,
            &parsed.symbols,
            &parsed.type_bindings,
        );
        let envs = std::collections::HashMap::from([(path.to_string(), env)]);
        for (line, caller, first, second) in [
            (8, "nested", true, false),
            (9, "nested", false, true),
            (10, "nested", true, false),
            (14, "shadow", false, true),
            (17, "index", false, false),
            (18, "namedIndex", false, false),
            (19, "unknown", false, false),
        ] {
            let references = parsed
                .references
                .iter()
                .filter(|reference| {
                    reference.kind != ReferenceKind::Call
                        || reference.name != "Deliver"
                        || reference.start_line == line
                })
                .cloned()
                .collect();
            let files = vec![(path.to_string(), parsed.symbols.clone(), references)];
            let edges = resolve_references_with_context(
                &files,
                Language::Go,
                "repo:test:abc",
                &WorkspaceContext::default(),
                Some(&envs),
                None,
            );
            assert_eq!(
                qa_calls(&files, &edges, (path, caller), (path, "Deliver", 3)),
                first,
                "line {line}, First: {edges:#?}"
            );
            assert_eq!(
                qa_calls(&files, &edges, (path, caller), (path, "Deliver", 5)),
                second,
                "line {line}, Second: {edges:#?}"
            );
        }
    }

    #[test]
    fn go_slice_range_re_resolution_retargets_a_changed_factory_element() {
        for element_type in ["First", "Second"] {
            let source = format!(
                "package app\ntype First struct{{}}\nfunc (recv First) Deliver() {{}}\ntype Second struct{{}}\nfunc (recv Second) Deliver() {{}}\nfunc All() []{element_type} {{ return nil }}\nfunc use() {{ for _, element := range All() {{ element.Deliver() }} }}\n"
            );
            let (files, edges) = review_edges(&[("app.go", &source)], Language::Go);
            for (name, line) in [("First", 3), ("Second", 5)] {
                assert_eq!(
                    qa_calls(
                        &files,
                        &edges,
                        ("app.go", "use"),
                        ("app.go", "Deliver", line)
                    ),
                    name == element_type,
                    "changed return to {element_type}: {edges:#?}"
                );
            }
        }
    }

    #[test]
    fn go_range_shadow_does_not_borrow_outer_receiver_type_or_hide_it_after_loop() {
        let path = "app.go";
        let source = "package app\ntype Store struct{}\nfunc (s *Store) Get() {}\nfunc run(items []interface{}) {\n x := &Store{}\n for _, x := range items {\n  x.Get()\n }\n x.Get()\n}\n";
        let parsed = parse_source(Path::new(path), source).unwrap();
        let env = crate::types::TypeEnvironment::build(
            source,
            Language::Go,
            &parsed.symbols,
            &parsed.type_bindings,
        );
        let envs = std::collections::HashMap::from([(path.to_string(), env)]);
        // Resolve each call separately: edge deduplication must not let the
        // valid post-loop call conceal an invented edge from the inner call.
        for (line, should_resolve) in [(7, false), (9, true)] {
            let references = parsed
                .references
                .iter()
                .filter(|reference| {
                    reference.kind != ReferenceKind::Call
                        || reference.name != "Get"
                        || reference.start_line == line
                })
                .cloned()
                .collect();
            let files = vec![(path.to_string(), parsed.symbols.clone(), references)];
            let edges = resolve_references_with_context(
                &files,
                Language::Go,
                "repo:test:abc",
                &WorkspaceContext::default(),
                Some(&envs),
                None,
            );
            assert_eq!(
                qa_calls(&files, &edges, (path, "run"), (path, "Get", 3)),
                should_resolve,
                "call line {line}: {edges:#?}"
            );
        }
    }

    #[test]
    fn go_pointer_and_value_receivers_resolve_without_receiver_name_leakage() {
        let (files, edges) = review_edges(
            &[
                (
                    "store.go",
                    "package app\ntype Store struct{}\nfunc (s *Store) Get() int { return 1 }\n",
                ),
                (
                    "app.go",
                    "package app\nfunc pointer() { x := &Store{}; x.Get() }\nfunc value() { x := Store{}; x.Get() }\nfunc unknown(s interface{}) { s.Get() }\nfunc shadow() { x := &Store{}; { x := unknownValue(); x.Get() }; x.Get() }\n",
                ),
            ],
            Language::Go,
        );
        for caller in ["pointer", "value", "shadow"] {
            assert!(
                qa_calls(&files, &edges, ("app.go", caller), ("store.go", "Get", 3)),
                "{caller}: {edges:#?}"
            );
        }
        assert!(
            !qa_calls(
                &files,
                &edges,
                ("app.go", "unknown"),
                ("store.go", "Get", 3)
            ),
            "a method receiver's name must not type unrelated parameters: {edges:#?}"
        );
    }

    #[test]
    fn hash_filename_does_not_change_rust_return_positions_or_js_assignments() {
        for path in ["src/name.rs", "src/has#name.rs"] {
            let source = "struct Store;\nimpl Store {\n fn leaf(&self) {}\n}\nfn factory() -> Store { Store }\nfn use_store() { let db = factory(); db.leaf(); }\n";
            let (files, edges) = review_edges(&[(path, source)], Language::Rust);
            assert!(
                qa_calls(&files, &edges, (path, "use_store"), (path, "leaf", 3)),
                "{path}: {edges:#?}"
            );
        }
        for (language, extension) in [(Language::JavaScript, "js"), (Language::TypeScript, "ts")] {
            for basename in ["name", "has#name"] {
                let path = format!("src/{basename}.{extension}");
                let source = "class Store {\n leaf() {}\n}\nfunction use_store() { const db = new Store(); db.leaf(); }\nfunction unknown(db) { db.leaf(); }\n";
                let (files, edges) = review_edges(&[(path.as_str(), source)], language);
                assert!(
                    qa_calls(&files, &edges, (&path, "use_store"), (&path, "leaf", 2)),
                    "{path}: {edges:#?}"
                );
                assert!(
                    !qa_calls(&files, &edges, (&path, "unknown"), (&path, "leaf", 2)),
                    "unknown donor: {path}: {edges:#?}"
                );
            }
        }
    }

    #[test]
    fn go_factory_chain_and_assignment_keep_hash_filename_origins() {
        for path in ["pkg/name.go", "pkg/has#name.go", "pkg/#12/has##name.go"] {
            for split in [false, true] {
                let same = "package pkg\ntype Provider struct{}\nfunc NewProvider() Provider { return Provider{} }\nfunc (p Provider) Named() {}\nfunc PTChain() { NewProvider().Named() }\nfunc PTAssignment() { p := NewProvider(); p.Named() }\n";
                let declaration = "package pkg\ntype Provider struct{}\nfunc NewProvider() Provider { return Provider{} }\n";
                // Keep split files in the declaration's exact package directory.
                let dir = path.rsplit_once('/').unwrap().0;
                let method_path = format!("{dir}/methods.go");
                let caller_path = format!("{dir}/caller.go");
                let inputs = if split {
                    vec![
                        (path, declaration),
                        (
                            method_path.as_str(),
                            "package pkg\nfunc (p Provider) Named() {}\n",
                        ),
                        (
                            caller_path.as_str(),
                            "package pkg\nfunc PTChain() { NewProvider().Named() }\nfunc PTAssignment() { p := NewProvider(); p.Named() }\n",
                        ),
                    ]
                } else {
                    vec![(path, same)]
                };
                let (files, edges) = review_edges(&inputs, Language::Go);
                let caller_file = if split { caller_path.as_str() } else { path };
                let target_file = if split { method_path.as_str() } else { path };
                for caller in ["PTChain", "PTAssignment"] {
                    assert!(
                        qa_calls(
                            &files,
                            &edges,
                            (caller_file, caller),
                            (path, "NewProvider", 3)
                        ),
                        "{path}/{split}/{caller}: {edges:#?}"
                    );
                    assert!(
                        qa_calls(
                            &files,
                            &edges,
                            (caller_file, caller),
                            (target_file, "Named", if split { 2 } else { 4 })
                        ),
                        "{path}/{split}/{caller}: {edges:#?}"
                    );
                }
            }
        }
    }

    #[test]
    fn go_factory_call_chains_keep_declaring_package_for_scalar_returns() {
        for result in [
            "Provider",
            "*Provider",
            "(value *Provider)",
            "choice.Provider",
        ] {
            for foreign_homonym in [false, true] {
                let factory = format!(
                    "package maker\nimport choice \"types\"\nfunc New() {result} {{ panic(\"fixture\") }}\n"
                );
                let local_types = if result == "choice.Provider" {
                    "package maker\n"
                } else {
                    "package maker\ntype Provider struct{}\nfunc (p Provider) Named() {}\n"
                };
                let foreign = if foreign_homonym {
                    "package foreign\ntype Provider struct{}\nfunc (p Provider) Named() {}\n"
                } else {
                    "package foreign\n"
                };
                let imported_types = if result == "choice.Provider" {
                    "package types\ntype Provider struct{}\nfunc (p Provider) Named() {}\n"
                } else {
                    "package types\n"
                };
                let app = if foreign_homonym {
                    "package app\nimport factory \"maker\"\ntype Provider struct{}\nfunc (p Provider) Named() {}\nfunc chain() { factory.New().Named() }\nfunc assigned() { p := factory.New(); p.Named() }\n"
                } else {
                    "package app\nimport factory \"maker\"\nfunc chain() { factory.New().Named() }\nfunc assigned() { p := factory.New(); p.Named() }\n"
                };
                let (files, edges) = review_edges(
                    &[
                        ("maker/factory.go", &factory),
                        ("maker/types.go", local_types),
                        ("types/types.go", imported_types),
                        ("foreign/types.go", foreign),
                        ("app.go", app),
                    ],
                    Language::Go,
                );
                let target = if result == "choice.Provider" {
                    "types/types.go"
                } else {
                    "maker/types.go"
                };
                for caller in ["chain", "assigned"] {
                    assert!(
                        qa_calls(&files, &edges, ("app.go", caller), (target, "Named", 3)),
                        "{result}/{foreign_homonym}/{caller}: {edges:#?}"
                    );
                    assert!(qa_calls(
                        &files,
                        &edges,
                        ("app.go", caller),
                        ("maker/factory.go", "New", 3)
                    ));
                    if foreign_homonym {
                        assert!(!qa_calls(
                            &files,
                            &edges,
                            ("app.go", caller),
                            ("app.go", "Named", 4)
                        ));
                        assert!(!qa_calls(
                            &files,
                            &edges,
                            ("app.go", caller),
                            ("foreign/types.go", "Named", 3)
                        ));
                    }
                }
            }
        }
    }

    #[test]
    fn go_factory_call_chains_scope_each_intermediate_method_and_identifier() {
        let (files, edges) = review_edges(
            &[
                (
                    "maker/types.go",
                    "package maker\nimport chosen \"chosen\"\ntype Provider struct{}\nfunc New() *Provider { return nil }\nfunc (p Provider) Next() chosen.Provider { panic(\"fixture\") }\nfunc samePackage() { New().Next().Named() }\n",
                ),
                (
                    "chosen/types.go",
                    "package chosen\ntype Provider struct{}\nfunc (p Provider) Named() {}\n",
                ),
                (
                    "other/types.go",
                    "package other\ntype Provider struct{}\nfunc (p Provider) Next() Provider { panic(\"fixture\") }\nfunc (p Provider) Named() {}\n",
                ),
                (
                    "app.go",
                    "package app\nimport factory \"maker\"\nfunc chain() { factory.New().Next().Named() }\nfunc assigned() { p := factory.New(); p.Next().Named() }\nfunc parameter(p *factory.Provider) { p.Next().Named() }\n",
                ),
            ],
            Language::Go,
        );
        for (path, caller) in [
            ("app.go", "chain"),
            ("app.go", "assigned"),
            ("app.go", "parameter"),
            ("maker/types.go", "samePackage"),
        ] {
            assert!(
                qa_calls(
                    &files,
                    &edges,
                    (path, caller),
                    ("chosen/types.go", "Named", 3)
                ),
                "{caller}: {edges:#?}"
            );
            assert!(!qa_calls(
                &files,
                &edges,
                (path, caller),
                ("other/types.go", "Named", 4)
            ));
        }
    }

    #[test]
    fn go_factory_call_chains_refuse_unproven_returns_shadowing_and_private_steps() {
        let (files, edges) = review_edges(
            &[
                (
                    "maker/types.go",
                    "package maker\ntype Provider struct{}\nfunc (p Provider) Named() {}\nfunc (p Provider) hidden() Provider { return p }\nfunc New() Provider { panic(\"fixture\") }\nfunc Many() (Provider, error) { panic(\"fixture\") }\nfunc Slice() []Provider { return nil }\nfunc External() library.Provider { panic(\"fixture\") }\nfunc Unknown() interface{} { return nil }\n",
                ),
                (
                    "app.go",
                    "package app\nimport factory \"maker\"\nfunc multiple() { factory.Many().Named() }\nfunc slice() { factory.Slice().Named() }\nfunc external() { factory.External().Named() }\nfunc unknown() { factory.Unknown().Named() }\nfunc shadow(factory interface{}) { factory.New().Named() }\nfunc privateStep() { factory.New().hidden().Named() }\n",
                ),
            ],
            Language::Go,
        );
        for caller in [
            "multiple",
            "slice",
            "external",
            "unknown",
            "shadow",
            "privateStep",
        ] {
            assert!(
                !qa_calls(
                    &files,
                    &edges,
                    ("app.go", caller),
                    ("maker/types.go", "Named", 3)
                ),
                "{caller}: {edges:#?}"
            );
        }
    }

    #[test]
    fn go_factory_call_chains_do_not_take_wrong_package_or_ignored_method_donors() {
        let (files, edges) = review_edges(
            &[
                (
                    "maker/types.go",
                    "package maker\ntype Provider struct{}\nfunc New() Provider { return Provider{} }\nfunc (p Provider) Named() {}\n",
                ),
                (
                    "maker/wrong.go",
                    "package foreign\ntype Provider struct{}\nfunc (p Provider) Named() {}\n",
                ),
                (
                    "maker/ignored.go",
                    "//go:build ignore\n\npackage maker\ntype Provider struct{}\nfunc (p Provider) Named() {}\n",
                ),
                (
                    "app.go",
                    "package app\nimport factory \"maker\"\nfunc chain() { factory.New().Named() }\n",
                ),
            ],
            Language::Go,
        );
        assert!(
            qa_calls(
                &files,
                &edges,
                ("app.go", "chain"),
                ("maker/types.go", "Named", 4)
            ),
            "{edges:#?}"
        );
        assert!(!qa_calls(
            &files,
            &edges,
            ("app.go", "chain"),
            ("maker/wrong.go", "Named", 3)
        ));
        assert!(!qa_calls(
            &files,
            &edges,
            ("app.go", "chain"),
            ("maker/ignored.go", "Named", 5)
        ));
    }

    #[test]
    fn go_factory_call_chains_require_origin_agreement_not_just_type_spelling() {
        // Invalid duplicate declarations deliberately model an ambiguous graph:
        // their same-spelled Provider returns name different exact packages.
        let (files, edges) = review_edges(
            &[
                (
                    "maker/first.go",
                    "package maker\nimport picked \"one\"\nfunc New() picked.Provider { panic(\"fixture\") }\n",
                ),
                (
                    "maker/second.go",
                    "package maker\nimport picked \"two\"\nfunc New() picked.Provider { panic(\"fixture\") }\n",
                ),
                (
                    "one/types.go",
                    "package one\ntype Provider struct{}\nfunc (p Provider) Named() {}\n",
                ),
                (
                    "two/types.go",
                    "package two\ntype Provider struct{}\nfunc (p Provider) Named() {}\n",
                ),
                (
                    "app.go",
                    "package app\nimport factory \"maker\"\nfunc chain() { factory.New().Named() }\n",
                ),
            ],
            Language::Go,
        );
        for target in ["one/types.go", "two/types.go"] {
            assert!(
                !qa_calls(&files, &edges, ("app.go", "chain"), (target, "Named", 3)),
                "{edges:#?}"
            );
        }
    }

    #[test]
    fn go_factory_call_chains_use_unchanged_callee_metadata_and_retarget_return_alias() {
        for picked in ["one", "two"] {
            let factory = format!(
                "package maker\nimport picked \"{picked}\"\nfunc New() picked.Provider {{ panic(\"fixture\") }}\n"
            );
            let inputs = [
                ("maker/factory.go", factory.as_str()),
                (
                    "one/types.go",
                    "package one\ntype Provider struct{}\nfunc (p Provider) Named() {}\n",
                ),
                (
                    "two/types.go",
                    "package two\ntype Provider struct{}\nfunc (p Provider) Named() {}\n",
                ),
                (
                    "app.go",
                    "package app\nimport factory \"maker\"\nfunc renamedCaller() { factory.New().Named() }\n",
                ),
            ];
            let mut files = Vec::new();
            let mut envs = std::collections::HashMap::new();
            for (path, source) in inputs {
                let parsed = parse_source(Path::new(path), source).unwrap();
                if path == "app.go" {
                    envs.insert(
                        path.to_string(),
                        crate::types::TypeEnvironment::build(
                            source,
                            Language::Go,
                            &parsed.symbols,
                            &parsed.type_bindings,
                        ),
                    );
                }
                files.push((path.to_string(), parsed.symbols, parsed.references));
            }
            let edges = resolve_references_with_context(
                &files,
                Language::Go,
                "repo:test:abc",
                &WorkspaceContext::default(),
                Some(&envs),
                None,
            );
            for (package, target) in [("one", "one/types.go"), ("two", "two/types.go")] {
                assert_eq!(
                    qa_calls(
                        &files,
                        &edges,
                        ("app.go", "renamedCaller"),
                        (target, "Named", 3)
                    ),
                    package == picked,
                    "retarget {picked}: {edges:#?}"
                );
            }
        }
    }

    #[test]
    fn go_qualified_annotations_literals_and_exact_factory_returns_use_import_origin() {
        let (files, edges) = review_edges(
            &[
                (
                    "store/types.go",
                    "package store\ntype Store struct{}\nfunc New() *Store { return &Store{} }\n",
                ),
                (
                    "store/methods.go",
                    "package store\nfunc (r *Store) Get() int { return 1 }\n",
                ),
                (
                    "other/types.go",
                    "package other\ntype Store struct{}\nfunc (r *Store) Get() int { return 2 }\nfunc New() *Store { return &Store{} }\n",
                ),
                (
                    "app.go",
                    "package app\nimport alias \"store\"\nfunc parameter(x *alias.Store) { x.Get() }\nfunc variable() { var x *alias.Store; x.Get() }\nfunc literal() { x := &alias.Store{}; x.Get() }\nfunc factory() { x := alias.New(); x.Get() }\nfunc unknown(x interface{}) { x.Get() }\n",
                ),
            ],
            Language::Go,
        );
        for caller in ["parameter", "variable", "literal", "factory"] {
            assert!(
                qa_calls(
                    &files,
                    &edges,
                    ("app.go", caller),
                    ("store/methods.go", "Get", 2)
                ),
                "{caller}: {edges:#?}"
            );
            assert!(
                !qa_calls(
                    &files,
                    &edges,
                    ("app.go", caller),
                    ("other/types.go", "Get", 3)
                ),
                "foreign homonym: {edges:#?}"
            );
        }
        assert!(
            !qa_calls(
                &files,
                &edges,
                ("app.go", "unknown"),
                ("store/methods.go", "Get", 2)
            ),
            "{edges:#?}"
        );
    }

    #[test]
    fn qa_go_method_on_constructor_function_result_resolves() {
        let calc = "internal/calc/calc.go";
        let test = "internal/calc/calc_test.go";
        let (files, edges) = review_edges(
            &[
                (
                    calc,
                    "package calc\n\ntype Impl struct{ base int }\n\nfunc New(b int) *Impl { return &Impl{base: b} }\n\nfunc (i *Impl) Add(a, b int) int { return i.base + a + b }\n",
                ),
                (
                    test,
                    "package calc\n\nimport \"testing\"\n\nfunc TestAdd(t *testing.T) {\n\tif New(1).Add(1, 2) != 4 {\n\t\tt.Fatal(\"bad\")\n\t}\n}\n",
                ),
            ],
            Language::Go,
        );
        assert!(
            qa_calls(&files, &edges, (test, "TestAdd"), (calc, "Add", 7)),
            "{edges:#?}"
        );
    }

    #[test]
    fn qa_rust_turbofish_calls_resolve_and_keep_their_receiver() {
        let path = "src/lib.rs";
        let source = "pub struct P;\nimpl P {\n    pub fn make<T>() -> P { P }\n}\nfn generic<T>(t: T) -> T { t }\nfn parse() {}\npub fn entry(x: &str) {\n    generic::<u8>(1);\n    P::make::<u8>();\n    let _ = x.parse::<u32>();\n}\n";
        let (files, edges) = review_edges(&[(path, source)], Language::Rust);
        assert!(
            qa_calls(&files, &edges, (path, "entry"), (path, "generic", 5)),
            "{edges:#?}"
        );
        assert!(
            qa_calls(&files, &edges, (path, "entry"), (path, "make", 3)),
            "{edges:#?}"
        );
        assert!(
            !qa_calls(&files, &edges, (path, "entry"), (path, "parse", 6)),
            "`x.parse::<u32>()` is a method call, not the free fn `parse`: {edges:#?}"
        );
    }

    #[test]
    fn qa_new_expression_links_the_class_in_every_language() {
        for (path, language, source, class_line) in [
            (
                "src/A.java",
                Language::Java,
                "class Foo { int bar() { return 1; } }\nclass User { void go() { new Foo(); } }\n",
                1,
            ),
            (
                "src/A.cs",
                Language::CSharp,
                "class Foo { public int Bar() { return 1; } }\nclass User { public void Go() { new Foo(); } }\n",
                1,
            ),
            (
                "src/a.php",
                Language::Php,
                "<?php\nclass Foo { public function bar() { return 1; } }\nfunction go() { return new Foo(); }\n",
                2,
            ),
            (
                "src/a.cpp",
                Language::Cpp,
                "class Foo { public: int bar() { return 1; } };\nvoid go() { Foo* f = new Foo(); }\n",
                1,
            ),
            (
                "src/A.scala",
                Language::Scala,
                "class Foo { def bar(): Int = 1 }\nobject User { def go(): Unit = { new Foo() } }\n",
                1,
            ),
            (
                "src/A.groovy",
                Language::Groovy,
                "class Foo { int bar() { return 1 } }\nclass User { void go() { new Foo() } }\n",
                1,
            ),
            (
                "lib/a.dart",
                Language::Dart,
                "class Foo { int bar() { return 1; } }\nvoid go() { new Foo(); }\n",
                1,
            ),
        ] {
            let (files, edges) = review_edges(&[(path, source)], language);
            let target = symbol_uid("repo:test:abc", path, "Foo", class_line);
            assert!(
                edges
                    .iter()
                    .any(|edge| edge.edge_type == EdgeType::Calls && edge.target_uid == target),
                "{path}: `new Foo()` must link the class: {:#?} {edges:#?}",
                files[0].2
            );
        }
    }

    #[test]
    fn qa_php_member_call_on_new_bound_variable_resolves() {
        let path = "c.php";
        let source = "<?php\nclass Foo { public function bar() { return 1; } }\nfunction mk() { $f = new Foo(); return $f->bar(); }\n";
        let (files, edges) = review_edges(&[(path, source)], Language::Php);
        assert!(
            qa_calls(&files, &edges, (path, "mk"), (path, "bar", 2)),
            "{edges:#?}"
        );
    }

    #[test]
    fn qa_ts_bare_decorator_references_the_decorator_function() {
        let path = "a.ts";
        let source = "function log(target: any, key: string) {}\nexport class C {\n  @log\n  plus(n: number): number { return n; }\n}\n";
        let (files, edges) = review_edges(&[(path, source)], Language::TypeScript);
        let log = uid(&files, path, "log");
        assert!(
            edges
                .iter()
                .any(|edge| edge.edge_type == EdgeType::Calls && edge.target_uid == log),
            "{edges:#?}"
        );
    }

    #[test]
    fn qa_rust_use_after_mod_declaration_is_not_owned_by_the_module() {
        let lib = "src/lib.rs";
        let util = "src/util.rs";
        let (files, edges) = review_edges(
            &[
                (
                    lib,
                    "mod util;\nuse util::double;\n\npub fn entry() -> i32 { double(2) }\n",
                ),
                (util, "pub fn double(x: i32) -> i32 { x * 2 }\n"),
            ],
            Language::Rust,
        );
        let module = uid(&files, lib, "util");
        assert!(
            !edges.iter().any(|edge| edge.source_uid == module),
            "`mod util;` has no body and owns nothing: {edges:#?}"
        );
        assert!(
            qa_calls(&files, &edges, (lib, "entry"), (util, "double", 1)),
            "{edges:#?}"
        );
    }

    #[test]
    fn qa_go_build_variants_survive_an_incremental_pass_without_package_clauses() {
        let node = "node.go";
        let js = "node_js.go";
        let net = "node_net.go";
        let mut files = parsed_files(&[
            (
                node,
                "package uuid\n\nfunc NodeID() string {\n\treturn getHardwareInterface(\"\")\n}\n",
            ),
            (
                js,
                "//go:build js\n\npackage uuid\n\nfunc getHardwareInterface(name string) string { return \"\" }\n",
            ),
            (
                net,
                "//go:build !js\n\npackage uuid\n\nfunc getHardwareInterface(name string) string { return name }\n",
            ),
        ]);
        // An incremental re-resolve passes unchanged files without references.
        for (path, _, references) in &mut files {
            if path != node {
                references.clear();
            }
        }
        let edges = resolve_references(&files, Language::Go, "repo:test:abc");
        for variant in [js, net] {
            assert!(
                qa_calls(
                    &files,
                    &edges,
                    (node, "NodeID"),
                    (variant, "getHardwareInterface", 5)
                ),
                "{variant}: {edges:#?}"
            );
        }
    }

    #[test]
    fn qa_go_build_variants_are_all_callees() {
        let node = "node.go";
        let js = "node_js.go";
        let net = "node_net.go";
        let (files, edges) = review_edges(
            &[
                (
                    node,
                    "package uuid\n\nfunc NodeID() string {\n\treturn getHardwareInterface(\"\")\n}\n",
                ),
                (
                    js,
                    "//go:build js\n\npackage uuid\n\nfunc getHardwareInterface(name string) string { return \"\" }\n",
                ),
                (
                    net,
                    "//go:build !js\n\npackage uuid\n\nfunc getHardwareInterface(name string) string { return name }\n",
                ),
            ],
            Language::Go,
        );
        for variant in [js, net] {
            assert!(
                qa_calls(
                    &files,
                    &edges,
                    (node, "NodeID"),
                    (variant, "getHardwareInterface", 5)
                ),
                "{variant}: {edges:#?}"
            );
        }
    }
}
