use nestweaver_parser::{RawReference, RawSymbol, ReferenceKind};
use nestweaver_schema::{
    EdgeType, Language, MatchType, ResolvedEdge, confidence_score, symbol_uid,
};

use crate::imports::build_import_graph;
use crate::util::parent_dir;

/// Resolve all non-import references across files into `ResolvedEdge`s.
///
/// Two-pass approach:
/// 1. Build the import graph (what each file imports/exports)
/// 2. For each non-import reference, find the target symbol using priority:
///    - Same file → SameFileExact confidence
///    - Direct imports → ImportResolved confidence
///    - Re-exports (one level deep) → ReExportResolved confidence
///    - Same package/directory → SamePackageFallback confidence
///    - No match → confidence 0.0, target_uid = "unresolved:{name}"
pub fn resolve_references(
    files: &[(String, Vec<RawSymbol>, Vec<RawReference>)],
    language: Language,
    repo_uid: &str,
) -> Vec<ResolvedEdge> {
    let graph = build_import_graph(files, language);

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

    let mut edges: Vec<ResolvedEdge> = Vec::new();

    for (file_path, symbols, references) in files {
        for reference in references {
            let edge_type = match reference.kind {
                ReferenceKind::Call => EdgeType::Calls,
                ReferenceKind::Extends => EdgeType::Extends,
                ReferenceKind::Implements => EdgeType::Implements,
                ReferenceKind::Includes => EdgeType::Includes,
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
            signature: format!("function {name}()"),
            content_hash: String::new(),
            is_entry_point: false,
            entry_point_kind: None,
            visibility: Visibility::Inferred,
            type_info: None,
        }
    }

    fn make_ref(name: &str, kind: ReferenceKind, line: u32) -> RawReference {
        RawReference {
            name: name.to_string(),
            kind,
            start_line: line,
            context: String::new(),
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
}
