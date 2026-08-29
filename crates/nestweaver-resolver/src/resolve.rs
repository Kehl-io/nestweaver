use nestweaver_parser::{RawReference, RawSymbol, ReferenceKind};
use nestweaver_schema::{
    EdgeEvidence, EdgeType, Language, MatchType, ResolvedEdge, SymbolKind, Visibility,
    confidence_score, symbol_uid,
};
use rayon::prelude::*;

use crate::imports::{ImportGraph, build_import_graph};
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
/// 3. Create IMPORTS edges from the import graph:
///    a) File-level: one IMPORTS edge per resolved import (first symbol → first symbol)
///    b) Named: link the enclosing source symbol to all exported target symbols
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
    let graph = build_import_graph(files, language, workspace_ctx);

    // Pre-sort symbols per file so find_enclosing_symbol's binary search invariant holds.
    // Tree-sitter guarantees sorted output in production, but callers (e.g. property tests)
    // may pass unsorted symbols, so we sort defensively here once per call.
    let sorted_symbols_per_file: Vec<Vec<&RawSymbol>> = files
        .iter()
        .map(|(_, symbols, _)| {
            let mut v: Vec<&RawSymbol> = symbols.iter().collect();
            v.sort_by_key(|s| s.start_line);
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

    // Build a parent-type lookup from Extends references for MRO walk.
    // Maps child type name → list of parent type names.
    let extends_map: std::collections::HashMap<String, Vec<String>> = {
        let mut map: std::collections::HashMap<String, Vec<String>> =
            std::collections::HashMap::new();
        for ((_, _, references), sorted_syms) in files.iter().zip(sorted_symbols_per_file.iter()) {
            for reference in references {
                if reference.kind == ReferenceKind::Extends
                    && let Some(sym) = find_enclosing_symbol(sorted_syms, reference.start_line)
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
            for reference in references {
                if let Some(edge) = resolve_single_reference(
                    file_path,
                    reference,
                    sorted_syms,
                    &symbol_map,
                    &extends_map,
                    &graph,
                    language,
                    repo_uid,
                    _type_envs,
                ) {
                    local_edges.push(edge);
                }
            }
            local_edges
        })
        .collect();

    let mut edges: Vec<ResolvedEdge> = ref_edges;

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

    // Build a file → sorted symbols lookup for find_enclosing_symbol in Pass 3b.
    let file_sorted_symbols: std::collections::HashMap<&str, &Vec<&RawSymbol>> = files
        .iter()
        .zip(sorted_symbols_per_file.iter())
        .map(|((path, _, _), sorted)| (path.as_str(), sorted))
        .collect();

    // Build a file → references lookup for import reference line matching.
    let file_refs: std::collections::HashMap<&str, &Vec<RawReference>> = files
        .iter()
        .map(|(path, _, refs)| (path.as_str(), refs))
        .collect();

    // ── Pass 3a: File-level IMPORTS edges ─────────────────────────────
    for (src_file, _specifier, tgt_file) in graph.all_resolved_imports() {
        if let Some(filter) = resolve_only
            && !filter.contains(src_file)
        {
            continue;
        }
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
                evidence: vec![EdgeEvidence {
                    kind: "structural".to_string(),
                    weight: confidence,
                    note: None,
                }],
            });
        }
    }

    // ── Pass 3b: Named-import IMPORTS edges (original precision pass) ─
    for (file_path, _symbols, _references) in files {
        if let Some(filter) = resolve_only
            && !filter.contains(file_path)
        {
            continue;
        }
        let imports = graph.imports_of(file_path);
        if imports.is_empty() {
            continue;
        }

        // A file with no symbols has nothing that could enclose an import, so
        // skip it before doing any per-import work. The symbol list itself is
        // no longer read here: since nw-103 this pass attributes an edge only
        // to a genuine enclosing symbol, never to the file's first declaration.
        if file_symbols
            .get(file_path.as_str())
            .is_none_or(|syms| syms.is_empty())
        {
            continue;
        }

        let source_sorted_syms: &[&RawSymbol] = match file_sorted_symbols.get(file_path.as_str()) {
            Some(syms) if !syms.is_empty() => syms.as_slice(),
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

            // Attribute a named-import edge only when the import genuinely sits
            // INSIDE a symbol — e.g. a dynamic `import()` in a function body.
            //
            // A top-level import has no enclosing symbol. Falling back to the
            // file's first declaration (nw-103) made that arbitrary symbol look
            // like the file's dependency hub: this pass then fans out to every
            // non-private symbol in the target file, so a 3-reference,
            // never-exported string constant acquired 830 out-edges and ranked
            // #5 in a 158k-symbol graph. Pass 3a already emits a file-level
            // proxy edge per import, so connectivity does not depend on this.
            let source_sym =
                import_line.and_then(|line| find_enclosing_symbol(source_sorted_syms, line));

            let source_uid = match source_sym {
                Some(sym) => symbol_uid(repo_uid, file_path, &sym.name, sym.start_line),
                None => continue,
            };

            // Get all non-private symbols in the target file.
            let target_symbols = match file_symbols.get(target_file.as_str()) {
                Some(syms) => *syms,
                None => continue,
            };

            let visible: Vec<&RawSymbol> = target_symbols
                .iter()
                .filter(|s| !matches!(s.visibility, Visibility::Private))
                .collect();

            if visible.is_empty() {
                continue;
            }

            // nw-153: honour the import's named binding. A specifier that names
            // one item -- `use crate::publication::ArtifactKind` -- must produce
            // ONE edge to that item, not one edge per symbol in the target file.
            //
            // The fan-out made backup_artifact_contract, whose body contains a
            // single `use`, an importer of all 64 symbols in publication.rs
            // including rollback_current and compare_and_swap_current. That is
            // why `impact rollback_current` surfaced unrelated backup code while
            // missing its real callers.
            //
            // The bound name is the specifier's last path segment. Languages
            // whose specifier names a MODULE rather than an item (JS `./helper`,
            // Python `os.path`) will not match a symbol, and those keep the
            // existing every-visible-symbol behaviour so connectivity is
            // unchanged for them.
            let bound_name = specifier
                .rsplit([':', '/', '.'])
                .find(|segment| !segment.is_empty());
            let named: Option<&&RawSymbol> =
                bound_name.and_then(|name| visible.iter().find(|candidate| candidate.name == name));

            let exported: Vec<&RawSymbol> = match named {
                Some(symbol) => vec![*symbol],
                None => visible,
            };

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
                    evidence: vec![EdgeEvidence {
                        kind: "structural".to_string(),
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

/// Resolve a single non-import reference to a `ResolvedEdge`, or return `None`
/// if the reference should be skipped (e.g. Import/Uses kind, no enclosing symbol).
///
/// This is extracted from the main loop body so it can be called from parallel
/// iterators without needing labeled `continue` across closure boundaries.
#[allow(clippy::too_many_arguments)]
fn resolve_single_reference(
    file_path: &str,
    reference: &RawReference,
    sorted_syms: &[&RawSymbol],
    symbol_map: &std::collections::HashMap<String, Vec<(&str, &RawSymbol)>>,
    extends_map: &std::collections::HashMap<String, Vec<String>>,
    graph: &ImportGraph,
    language: Language,
    repo_uid: &str,
    type_envs: Option<&std::collections::HashMap<String, crate::types::TypeEnvironment>>,
) -> Option<ResolvedEdge> {
    let edge_type = match reference.kind {
        ReferenceKind::Call => EdgeType::Calls,
        ReferenceKind::Extends => EdgeType::Extends,
        ReferenceKind::Implements => EdgeType::Implements,
        ReferenceKind::Includes => EdgeType::Includes,
        ReferenceKind::TypeRef => EdgeType::Uses,
        ReferenceKind::ReadAccess | ReferenceKind::WriteAccess => EdgeType::Accesses,
        ReferenceKind::Import | ReferenceKind::ImportAlias | ReferenceKind::Uses => return None,
    };

    let source_sym = find_enclosing_symbol(sorted_syms, reference.start_line)?;
    let source_uid = symbol_uid(repo_uid, file_path, &source_sym.name, source_sym.start_line);

    // ── Type-aware resolution for member calls with known receiver type ──
    if edge_type == EdgeType::Calls
        && let Some(ref receiver) = reference.receiver
        && let Some(envs) = type_envs
        && let Some(env) = envs.get(file_path)
    {
        let receiver_type = if receiver == "self" || receiver == "this" || receiver == "$this" {
            env.lookup_self(reference.start_line)
        } else if receiver.contains('.') {
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

            // Direct match on the receiver type
            if let Some(candidates) = symbol_map.get(method_name.as_str())
                && let Some((candidate_file, sym)) = candidates
                    .iter()
                    .find(|(_, s)| s.parent_name.as_deref() == Some(type_name.as_str()))
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
                        note: Some(format!("{} -> {}", receiver, type_name)),
                    }],
                });
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

                                if let Some(candidates) = symbol_map.get(method_name.as_str())
                                    && let Some((cf, sym)) = candidates.iter().find(|(_, s)| {
                                        s.parent_name.as_deref() == Some(parent.as_str())
                                    })
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
            // Type was known but method not found on that type or ancestors.
            // Fall through to name-based resolution.
        }
    }

    let name = &reference.name;

    // Priority 1: Same file. A local symbol shadows an import alias of the
    // same name, so this check runs on the reference's own name, before any
    // alias rewriting.
    if let Some(syms) = symbol_map.get(name.as_str())
        && let Some((_, sym)) = syms.iter().find(|(f, _)| *f == file_path)
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

    // Aliased import (`use path::to::original as name;`): the binding records
    // the original name and the file it was imported from.
    let binding = graph
        .bindings_of(file_path)
        .into_iter()
        .find(|b| b.local_name == *name);
    if let Some(binding) = binding
        && let Some(syms) = symbol_map.get(binding.original_name.as_str())
        && let Some((_, sym)) = syms.iter().find(|(f, _)| f == &binding.source_file)
    {
        let target_uid = symbol_uid(repo_uid, &binding.source_file, &sym.name, sym.start_line);
        let confidence = confidence_score(MatchType::ImportResolved, language);
        return Some(ResolvedEdge {
            source_uid,
            target_uid,
            edge_type,
            confidence,
            link_type: None,
            evidence: vec![EdgeEvidence {
                kind: "import_alias".to_string(),
                weight: confidence,
                note: Some(format!("{} -> {}", name, binding.original_name)),
            }],
        });
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
    // Gated on the qualifier containing `::` so this only fires for a genuine
    // multi-segment path. A bare receiver -- a JS variable in `store.method()`,
    // or the type in `HashMap::new()` -- is excluded, because matching those
    // against a same-named file would invent edges rather than recover them.
    if let Some(qualifier) = reference.receiver.as_deref()
        && qualifier.contains("::")
        && let Some(syms) = &candidates
        && let Some(module) = qualifier.rsplit("::").find(|segment| !segment.is_empty())
    {
        let mut qualified: Vec<_> = syms
            .iter()
            .filter(|(candidate_file, _)| {
                candidate_file
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

    // nw-308 / nw-327: the receiver gate. See `receiver_denotes` -- the nw-150
    // fix put exactly this test in, but only on Priority 4, the WEAKEST tier.
    // Priorities 2 and 3 return first and were ungated, so importing ANY symbol
    // from a file donated every bare method name in it: `.collect()` in
    // `tools.rs` bound to a private `SegmentCollector::collect` in
    // `tantivy_index.rs` purely because `tools.rs:30` imports `SearchTotal`
    // from that file. The comment at the Priority 4 tier below is a verbatim
    // description of this bug at a different tier.
    let value_receiver = reference
        .receiver
        .as_deref()
        .filter(|receiver| !receiver.contains("::"));

    // Priority 2: Direct imports
    let mut imports = graph.imports_of(file_path);
    imports.sort_by(|(_, a), (_, b)| a.cmp(b));
    for (_, imported_file) in &imports {
        if let Some(syms) = &candidates
            && let Some((_, sym)) = syms
                .iter()
                .find(|(f, sym)| f == imported_file && receiver_denotes(f, sym, value_receiver))
        {
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
    }

    // Priority 3: Re-exports
    for (_, imported_file) in &imports {
        let mut transitive_imports = graph.imports_of(imported_file);
        transitive_imports.sort_by(|(_, a), (_, b)| a.cmp(b));
        for (_, transitive_file) in &transitive_imports {
            if let Some(syms) = &candidates
                && let Some((_, sym)) = syms.iter().find(|(f, sym)| {
                    f == transitive_file && receiver_denotes(f, sym, value_receiver)
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
                *candidate_file != file_path && parent_dir(candidate_file) == same_dir
            })
            .filter(|(candidate_file, sym)| receiver_denotes(candidate_file, sym, value_receiver))
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

    // No match → unresolved
    Some(ResolvedEdge {
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
    })
}

/// How many re-export hops the Priority 3 tier walks.
///
/// nw-323: one hop was not enough for a real TypeScript barrel
/// (`common/errors.ts -> errors/index.ts -> http-errors.ts` is two), and an
/// unbounded walk would reinstate exactly the fan-out that nw-103 and nw-153
/// exist to prevent. Three covers the observed chains with headroom.
const REEXPORT_MAX_HOPS: usize = 3;

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
    if stem == Some(denoted) {
        return true;
    }
    sym.parent_name.as_deref() == Some(denoted)
}

/// Find the enclosing symbol: the innermost symbol whose span contains the
/// reference line (`start_line <= ref_line <= end_line`).
///
/// Requires `symbols` to be sorted by `start_line` ascending (tree-sitter LR-parser guarantee).
/// Binary search finds the last symbol starting at or before `ref_line`; if that
/// symbol's span does not contain the line (e.g. Python module-level statements
/// like `if __name__ == '__main__':` after the last `def`), walk back to an
/// earlier symbol whose span does — or return `None` when the reference is
/// module-level code that belongs to no symbol.
///
/// Degenerate-span fallback: the regex-based parsers (astro, svelte, vue,
/// cobol) emit one-line spans (`end_line == start_line`) while emitting Call
/// references on later lines, so no span can ever contain those refs. When no
/// span contains the line, attribute the reference to the nearest preceding
/// symbol with a degenerate span (`end_line <= start_line`). Symbols with
/// real spans that ended before `ref_line` still yield `None` — module-level
/// code belongs to no symbol.
fn find_enclosing_symbol<'a>(symbols: &'a [&'a RawSymbol], ref_line: u32) -> Option<&'a RawSymbol> {
    debug_assert!(
        symbols
            .windows(2)
            .all(|w| w[0].start_line <= w[1].start_line),
        "find_enclosing_symbol requires symbols sorted by start_line"
    );
    if symbols.is_empty() {
        return None;
    }
    let idx = symbols.partition_point(|s| s.start_line <= ref_line);
    if idx == 0 {
        return None;
    }
    if let Some(enclosing) = symbols[..idx]
        .iter()
        .rev()
        .find(|s| ref_line <= s.end_line.max(s.start_line))
        .copied()
    {
        return Some(enclosing);
    }
    symbols[..idx]
        .iter()
        .rev()
        .find(|s| s.end_line <= s.start_line)
        .copied()
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
                    make_ref("helperFn", ReferenceKind::Call, 10), // receiver: None
                ],
            ),
            (
                "src/helper.js".to_string(),
                vec![make_symbol("helperFn", 1)],
                vec![],
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
                vec![make_ref("./store", ReferenceKind::Import, 1), call],
            ),
            (
                "src/store.js".to_string(),
                vec![make_symbol("query", 5)],
                vec![],
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
    fn top_level_import_creates_one_file_level_proxy_edge() {
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
            1,
            "a top-level import should yield exactly one file-level proxy edge, \
             not one per export (nw-103 fan-out); got: {import_edges:?}"
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
            from_storage_key.len() <= 1,
            "STORAGE_KEY is declared after the import block and must not inherit \
             the file's imports; at most a single file-level proxy edge is \
             acceptable. Got {} IMPORTS edges: {:#?}",
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
            let binary = find_enclosing_symbol(&sorted, ref_line).map(|s| s.start_line);
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
            find_enclosing_symbol(&sorted, ref_line).map(|s| s.start_line)
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
            scope_chain: None,
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
            scope_chain: None,
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
