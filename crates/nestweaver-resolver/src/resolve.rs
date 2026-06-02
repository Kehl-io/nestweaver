use nestweaver_parser::{RawReference, RawSymbol, ReferenceKind};
use nestweaver_schema::{
    EdgeType, Language, MatchType, ResolvedEdge, SymbolKind, Visibility, confidence_score,
    symbol_uid,
};

use crate::imports::build_import_graph;
use crate::util::parent_dir;
use crate::workspace::WorkspaceContext;

/// Resolve all references across files into `ResolvedEdge`s.
///
/// Three-pass approach:
/// 1. Build the import graph (what each file imports/exports)
/// 2. For each non-import reference, find the target symbol using priority:
///    - Same file → SameFileExact confidence
///    - Direct imports → ImportResolved confidence
///    - Re-exports (one level deep) → ReExportResolved confidence
///    - Same package/directory → SamePackageFallback confidence
///    - No match → confidence 0.0, target_uid = "unresolved:{name}"
/// 3. Create IMPORTS edges from the import graph:
///    a) File-level: one IMPORTS edge per resolved import (first symbol → first symbol)
///    b) Named: link the enclosing source symbol to all exported target symbols
///
/// Edges are deduplicated by (source_uid, target_uid, edge_type).
///
/// The optional `workspace_ctx` enables resolution of monorepo workspace
/// package imports and tsconfig path aliases for JS/TS files.
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
) -> Vec<ResolvedEdge> {
    let graph = build_import_graph(files, language, workspace_ctx);

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

    // Build a parent-type lookup from Extends references for MRO walk.
    // Maps child type name → list of parent type names.
    let extends_map: std::collections::HashMap<String, Vec<String>> = {
        let mut map: std::collections::HashMap<String, Vec<String>> =
            std::collections::HashMap::new();
        for (_, symbols, references) in files {
            for reference in references {
                if reference.kind == ReferenceKind::Extends
                    && let Some(sym) = find_enclosing_symbol(symbols, reference.start_line)
                    && matches!(
                        sym.kind,
                        SymbolKind::Class
                            | SymbolKind::Enum
                            | SymbolKind::Interface
                            | SymbolKind::Trait
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

    let mut edges: Vec<ResolvedEdge> = Vec::new();

    for (file_path, symbols, references) in files {
        'ref_loop: for reference in references {
            let edge_type = match reference.kind {
                ReferenceKind::Call => EdgeType::Calls,
                ReferenceKind::Extends => EdgeType::Extends,
                ReferenceKind::Implements => EdgeType::Implements,
                ReferenceKind::Includes => EdgeType::Includes,
                ReferenceKind::TypeRef => EdgeType::Uses,
                ReferenceKind::ReadAccess | ReferenceKind::WriteAccess => EdgeType::Accesses,
                ReferenceKind::Import | ReferenceKind::Uses => continue,
            };

            // Find the enclosing symbol: symbol in same file with largest start_line <= reference.start_line
            let source_sym = find_enclosing_symbol(symbols, reference.start_line);
            let source_uid = match source_sym {
                Some(sym) => symbol_uid(repo_uid, file_path, &sym.name, sym.start_line),
                None => {
                    // No enclosing symbol — skip this reference
                    continue;
                }
            };

            // ── Type-aware resolution for member calls with known receiver type ──
            if edge_type == EdgeType::Calls
                && let Some(ref receiver) = reference.receiver
                && let Some(envs) = _type_envs
                && let Some(env) = envs.get(file_path.as_str())
            {
                let receiver_type =
                    if receiver == "self" || receiver == "this" || receiver == "$this" {
                        env.lookup_self(reference.start_line)
                    } else if receiver.contains('.') {
                        // Decompose chained receivers: a.b.method() → look up
                        // the type of `a`, then use it. For chains deeper than
                        // 2 segments (a.b.c.method), only the first segment's
                        // type is resolved — field-of-type resolution would
                        // require a full type system, so we fall through to
                        // name-based resolution for deeper chains.
                        let first = receiver.split('.').next().unwrap_or(receiver);
                        if first == "self" || first == "this" || first == "$this" {
                            env.lookup_self(reference.start_line)
                        } else {
                            env.lookup(first, reference.start_line)
                        }
                    } else {
                        env.lookup(receiver, reference.start_line)
                    };

                if let Some(binding) = receiver_type {
                    let method_name = &reference.name;
                    let type_name = &binding.type_name;

                    if let Some(candidates) = symbol_map.get(method_name.as_str())
                        && let Some((candidate_file, sym)) = candidates
                            .iter()
                            .find(|(_, s)| s.parent_name.as_deref() == Some(type_name.as_str()))
                    {
                        let target_uid =
                            symbol_uid(repo_uid, candidate_file, &sym.name, sym.start_line);
                        let confidence = binding.confidence.min(0.95);
                        edges.push(ResolvedEdge {
                            source_uid: source_uid.clone(),
                            target_uid,
                            edge_type,
                            confidence,
                            link_type: None,
                        });
                        continue 'ref_loop;
                    }
                    // MRO walk: check parent types via inheritance chain
                    {
                        let mut current_types = vec![type_name.clone()];
                        let mut visited = std::collections::HashSet::new();
                        visited.insert(type_name.clone());
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

                                        if let Some(candidates) =
                                            symbol_map.get(method_name.as_str())
                                            && let Some((cf, sym)) =
                                                candidates.iter().find(|(_, s)| {
                                                    s.parent_name.as_deref()
                                                        == Some(parent.as_str())
                                                })
                                        {
                                            let target_uid =
                                                symbol_uid(repo_uid, cf, &sym.name, sym.start_line);
                                            let conf = binding.confidence
                                                * 0.95_f32.powi((depth + 1) as i32);
                                            edges.push(ResolvedEdge {
                                                source_uid: source_uid.clone(),
                                                target_uid,
                                                edge_type,
                                                confidence: conf,
                                                link_type: None,
                                            });
                                            continue 'ref_loop;
                                        }
                                        next_types.push(parent.clone());
                                    }
                                }
                            }
                            current_types = next_types;
                            depth += 1;
                        }
                    }
                    // Type was known but method not found on that type or ancestors.
                    // Fall through to name-based resolution.
                }
            }

            let name = &reference.name;

            // Check if the reference name is a local alias introduced by an aliased import.
            // If so, resolve using the original exported name instead.
            let effective_name = {
                let bindings = graph.bindings_of(file_path);
                if let Some(binding) = bindings.iter().find(|b| b.local_name == *name) {
                    binding.original_name.clone()
                } else {
                    name.clone()
                }
            };

            let candidates = symbol_map.get(effective_name.as_str());

            // Priority 1: Same file
            if let Some(syms) = &candidates
                && let Some((_, sym)) = syms.iter().find(|(f, _)| *f == file_path.as_str())
            {
                let target_uid = symbol_uid(repo_uid, file_path, &sym.name, sym.start_line);
                let confidence = confidence_score(MatchType::SameFileExact, language);
                edges.push(ResolvedEdge {
                    source_uid,
                    target_uid,
                    edge_type,
                    confidence,
                    link_type: None,
                });
                continue;
            }

            // Priority 2: Direct imports — find files directly imported by this file
            let mut imports = graph.imports_of(file_path);
            imports.sort_by(|(_, a), (_, b)| a.cmp(b));
            let mut found = false;
            'import_search: for (_, imported_file) in &imports {
                if let Some(syms) = &candidates
                    && let Some((_, sym)) = syms.iter().find(|(f, _)| f == imported_file)
                {
                    let target_uid = symbol_uid(repo_uid, imported_file, &sym.name, sym.start_line);
                    let confidence = confidence_score(MatchType::ImportResolved, language);
                    edges.push(ResolvedEdge {
                        source_uid: source_uid.clone(),
                        target_uid,
                        edge_type,
                        confidence,
                        link_type: None,
                    });
                    found = true;
                    break 'import_search;
                }
            }
            if found {
                continue;
            }

            // Priority 3: Re-exports — files imported by our imports that export the name
            let mut found = false;
            'reexport_search: for (_, imported_file) in &imports {
                let mut transitive_imports = graph.imports_of(imported_file);
                transitive_imports.sort_by(|(_, a), (_, b)| a.cmp(b));
                for (_, transitive_file) in &transitive_imports {
                    if let Some(syms) = &candidates
                        && let Some((_, sym)) = syms.iter().find(|(f, _)| f == transitive_file)
                    {
                        let target_uid =
                            symbol_uid(repo_uid, transitive_file, &sym.name, sym.start_line);
                        let confidence = confidence_score(MatchType::ReExportResolved, language);
                        edges.push(ResolvedEdge {
                            source_uid: source_uid.clone(),
                            target_uid,
                            edge_type,
                            confidence,
                            link_type: None,
                        });
                        found = true;
                        break 'reexport_search;
                    }
                }
            }
            if found {
                continue;
            }

            // Priority 4: Same package/directory — files in the same directory
            let same_dir = parent_dir(file_path);
            let mut found = false;
            if let Some(syms) = &candidates {
                let mut same_pkg: Vec<_> = syms
                    .iter()
                    .filter(|(candidate_file, _)| {
                        *candidate_file != file_path.as_str()
                            && parent_dir(candidate_file) == same_dir
                    })
                    .collect();
                same_pkg.sort_by_key(|(path, _)| *path);
                if let Some((candidate_file, sym)) = same_pkg.into_iter().next() {
                    let target_uid =
                        symbol_uid(repo_uid, candidate_file, &sym.name, sym.start_line);
                    let confidence = confidence_score(MatchType::SamePackageFallback, language);
                    edges.push(ResolvedEdge {
                        source_uid: source_uid.clone(),
                        target_uid,
                        edge_type,
                        confidence,
                        link_type: None,
                    });
                    found = true;
                }
            }
            if found {
                continue;
            }

            // No match → unresolved
            let target_uid = format!("unresolved:{name}");
            edges.push(ResolvedEdge {
                source_uid,
                target_uid,
                edge_type,
                confidence: 0.0,
                link_type: None,
            });
        }
    }

    // ── Pass 3: Create IMPORTS edges from the import graph ──────────────
    //
    // Two sub-passes:
    //   3a) File-level IMPORTS edges: for every resolved import, create one
    //       IMPORTS edge from the first symbol in the source file to the first
    //       symbol in the target file. This is a file-level proxy that ensures
    //       connectivity even when named-binding data is unavailable.
    //   3b) Named-import IMPORTS edges (original logic): for each resolved
    //       import, link the enclosing source symbol to all non-private symbols
    //       in the target file. These are more precise when available.
    //
    // Both sub-passes are purely additive. Edges are deduplicated at the end.

    // Build a file → symbols lookup for target file symbol access.
    let file_symbols: std::collections::HashMap<&str, &Vec<RawSymbol>> = files
        .iter()
        .map(|(path, syms, _)| (path.as_str(), syms))
        .collect();

    // Build a file → references lookup for import reference line matching.
    let file_refs: std::collections::HashMap<&str, &Vec<RawReference>> = files
        .iter()
        .map(|(path, _, refs)| (path.as_str(), refs))
        .collect();

    // ── Pass 3a: File-level IMPORTS edges ─────────────────────────────
    for (src_file, _specifier, tgt_file) in graph.all_resolved_imports() {
        let src_sym = file_symbols.get(src_file).and_then(|syms| syms.first());
        let tgt_sym = file_symbols.get(tgt_file).and_then(|syms| syms.first());

        if let (Some(src), Some(tgt)) = (src_sym, tgt_sym) {
            let source_uid = symbol_uid(repo_uid, src_file, &src.name, src.start_line);
            let target_uid = symbol_uid(repo_uid, tgt_file, &tgt.name, tgt.start_line);
            let confidence = confidence_score(MatchType::ImportResolved, language);
            edges.push(ResolvedEdge {
                source_uid,
                target_uid,
                edge_type: EdgeType::Imports,
                confidence,
                link_type: None,
            });
        }
    }

    // ── Pass 3b: Named-import IMPORTS edges (original precision pass) ─
    for (file_path, _symbols, _references) in files {
        let imports = graph.imports_of(file_path);
        if imports.is_empty() {
            continue;
        }

        let source_symbols = match file_symbols.get(file_path.as_str()) {
            Some(syms) if !syms.is_empty() => *syms,
            _ => continue,
        };

        let empty_refs = Vec::new();
        let source_refs = file_refs
            .get(file_path.as_str())
            .copied()
            .unwrap_or(&empty_refs);

        for (specifier, target_file) in &imports {
            // Find the import reference line to determine the enclosing source symbol.
            let import_line = source_refs
                .iter()
                .find(|r| {
                    matches!(
                        r.kind,
                        ReferenceKind::Import | ReferenceKind::Includes | ReferenceKind::Uses
                    ) && r.name == *specifier
                })
                .map(|r| r.start_line);

            // Use enclosing symbol at import line, or fall back to first symbol in file.
            let source_sym = import_line
                .and_then(|line| find_enclosing_symbol(source_symbols, line))
                .or_else(|| source_symbols.first());

            let source_uid = match source_sym {
                Some(sym) => symbol_uid(repo_uid, file_path, &sym.name, sym.start_line),
                None => continue,
            };

            // Get all non-private symbols in the target file.
            let target_symbols = match file_symbols.get(target_file.as_str()) {
                Some(syms) => *syms,
                None => continue,
            };

            let exported: Vec<&RawSymbol> = target_symbols
                .iter()
                .filter(|s| !matches!(s.visibility, Visibility::Private))
                .collect();

            if exported.is_empty() {
                continue;
            }

            let confidence = confidence_score(MatchType::ImportResolved, language);

            for target_sym in &exported {
                let target_uid = symbol_uid(
                    repo_uid,
                    target_file,
                    &target_sym.name,
                    target_sym.start_line,
                );
                edges.push(ResolvedEdge {
                    source_uid: source_uid.clone(),
                    target_uid,
                    edge_type: EdgeType::Imports,
                    confidence,
                    link_type: None,
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

/// Find the enclosing symbol: the symbol with the largest start_line that is <= reference line.
fn find_enclosing_symbol(symbols: &[RawSymbol], ref_line: u32) -> Option<&RawSymbol> {
    symbols
        .iter()
        .filter(|s| s.start_line <= ref_line)
        .max_by_key(|s| s.start_line)
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
            end_line: line,
            signature: format!("function {name}()"),
            content_hash: String::new(),
            is_entry_point: false,
            entry_point_kind: None,
            visibility: Visibility::Inferred,
            type_info: None,
            parent_name: None,
        }
    }

    fn make_ref(name: &str, kind: ReferenceKind, line: u32) -> RawReference {
        RawReference {
            name: name.to_string(),
            kind,
            start_line: line,
            context: String::new(),
            receiver: None,
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
                    make_ref("helperFn", ReferenceKind::Call, 10),
                ],
            ),
            (
                "src/helper.js".to_string(),
                vec![make_symbol("helperFn", 1)],
                vec![],
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

    #[test]
    fn creates_imports_edges_from_import_graph() {
        // main.js imports helper.js — should create IMPORTS edges
        // to all exported symbols in helper.js
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
        assert_eq!(
            import_edges.len(),
            2,
            "should create IMPORTS edges to both exported symbols; got: {import_edges:?}"
        );

        let expected_confidence = confidence_score(MatchType::ImportResolved, Language::JavaScript);
        for edge in &import_edges {
            assert!(
                (edge.confidence - expected_confidence).abs() < f32::EPSILON,
                "IMPORTS edge should have ImportResolved confidence"
            );
            assert!(
                !edge.target_uid.starts_with("unresolved:"),
                "IMPORTS target should be resolved"
            );
        }
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
            1,
            "should only create IMPORTS edge to non-private symbol; got: {import_edges:?}"
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
                ],
            ),
            (
                "src/helper.js".to_string(),
                vec![make_symbol("helperFn", 1)],
                vec![],
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
        // Two IMPORTS edges: file-level (init -> helperFn) + named (setup -> helperFn)
        assert_eq!(
            import_edges.len(),
            2,
            "expected 2 IMPORTS edges; got: {import_edges:?}"
        );

        // The named-import edge should use "setup" (line 10) as source
        let named_source = symbol_uid("repo:test:abc", "src/main.js", "setup", 10);
        assert!(
            import_edges.iter().any(|e| e.source_uid == named_source),
            "should have an IMPORTS edge from the enclosing symbol at the import line"
        );

        // The file-level edge should use "init" (line 1) as source
        let file_level_source = symbol_uid("repo:test:abc", "src/main.js", "init", 1);
        assert!(
            import_edges
                .iter()
                .any(|e| e.source_uid == file_level_source),
            "should have a file-level IMPORTS edge from the first symbol in the file"
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
        // File-level and named-import both resolve to (main -> helperFn), deduped to 1
        assert_eq!(
            import_edges.len(),
            1,
            "duplicate imports should be deduplicated; got: {import_edges:?}"
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
        // Two targets: main->helperFn and main->utilFn (file-level + named dedup to 2)
        assert_eq!(
            import_edges.len(),
            2,
            "should have IMPORTS edges to both target files; got: {import_edges:?}"
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
                        make_ref("helperFn", ReferenceKind::Call, 10),
                        make_ref("missingFn", ReferenceKind::Call, 15),
                    ],
                ),
                (
                    "src/helper.js".to_string(),
                    vec![make_symbol("helperFn", 1)],
                    vec![],
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
        foo_bar.kind = SymbolKind::Function;

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
        };

        // File B: class Baz with method bar (different parent)
        let mut baz_bar = make_symbol("bar", 5);
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
        };

        // File C: caller that does foo_instance.bar()
        let caller = make_symbol("caller", 1);
        let bar_call = RawReference {
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
        };

        let extends_ref = RawReference {
            name: "BaseClass".to_string(),
            kind: ReferenceKind::Extends,
            start_line: 30,
            context: String::new(),
            receiver: None,
        };

        // Caller file: child_instance.save()
        let caller = make_symbol("caller", 1);
        let save_call = RawReference {
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
        };
        let mut handle_method = make_symbol("handle", 10);
        handle_method.parent_name = Some("MyService".to_string());

        let chained_call = RawReference {
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
}
