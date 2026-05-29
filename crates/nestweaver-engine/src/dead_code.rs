//! Dead code detection via entry point reachability analysis.
//!
//! Walks forward from every entry point in the graph following
//! CALLS, IMPORTS, EXTENDS, IMPLEMENTS, and MEMBER_OF edges. Any
//! symbol not reached is potentially dead. Confidence scoring
//! accounts for visibility: private unreachable symbols are
//! high-confidence dead code; public ones could be library API.

use std::collections::{HashMap, HashSet, VecDeque};

use nestweaver_schema::SymbolKind;
use nestweaver_store::GraphStore;
use serde::Serialize;

use crate::manifest::ManifestInfo;

/// Confidence that a symbol is truly dead code.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
pub enum DeadCodeConfidence {
    /// Public symbol in a potentially library crate — could be used externally.
    Low,
    /// Public symbol in a non-library crate, or internal symbol with some
    /// ambiguity (e.g. `Inferred` visibility).
    Medium,
    /// Private/internal unreachable symbol — cannot be called externally.
    High,
}

impl DeadCodeConfidence {
    /// Parse from a CLI flag string (case-insensitive).
    pub fn from_str_loose(s: &str) -> Option<Self> {
        match s.to_lowercase().as_str() {
            "low" => Some(Self::Low),
            "medium" | "med" => Some(Self::Medium),
            "high" => Some(Self::High),
            _ => None,
        }
    }
}

impl std::fmt::Display for DeadCodeConfidence {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Low => write!(f, "low"),
            Self::Medium => write!(f, "medium"),
            Self::High => write!(f, "high"),
        }
    }
}

/// A symbol that was not reached from any entry point.
#[derive(Debug, Clone, Serialize)]
pub struct UnreachableSymbol {
    pub uid: String,
    pub name: String,
    pub kind: String,
    pub file_path: String,
    pub visibility: String,
    pub confidence: DeadCodeConfidence,
}

/// Summary result of the dead code detection pass.
#[derive(Debug, Clone, Serialize)]
pub struct DeadCodeResult {
    pub unreachable_symbols: Vec<UnreachableSymbol>,
    pub total_symbols: usize,
    pub reachable_symbols: usize,
    pub dead_percentage: f64,
    /// Number of symbols excluded from analysis (type-only symbols, `.d.ts`
    /// declarations, properties). These are not counted in `total_symbols`.
    pub excluded_count: usize,
}

/// Returns `true` for symbols that should be excluded from dead code analysis
/// because they are type-only constructs (erased at compile/runtime), live in
/// `.d.ts` declaration files, or are properties (often accessed dynamically).
fn is_excluded_from_dead_code(sym: &nestweaver_schema::Symbol) -> bool {
    matches!(
        sym.kind,
        SymbolKind::TypeAlias | SymbolKind::Interface | SymbolKind::Property
    ) || sym.file_path.ends_with(".d.ts")
}

/// Default minimum edge confidence for BFS traversal.
const DEFAULT_MIN_EDGE_CONFIDENCE: f32 = 0.3;

/// Edge confidence below which reachability is considered "weak".
/// Symbols reachable *only* via edges below this threshold are
/// reported as Medium confidence dead code rather than alive.
const WEAK_EDGE_THRESHOLD: f32 = 0.5;

/// Detect potentially dead code by walking forward from all entry points.
///
/// Algorithm:
/// 1. Collect every Symbol with `is_entry_point == true`.
/// 2. BFS forward from entry points following reachability edges,
///    skipping edges with confidence below `min_edge_confidence`.
/// 3. Symbols reachable only through low-confidence edges (< 0.5)
///    are reported as Medium confidence dead code instead of alive.
/// 4. Any symbol not in the visited set is reported as unreachable.
/// 5. Confidence is scored based on inferred visibility heuristics.
/// 6. Methods of dead classes are deduplicated (only the class is reported).
///
/// If `manifests` is provided, symbols whose file paths match `main`, `bin`,
/// or `exports` entries in a package.json manifest are treated as additional
/// entry points.
///
/// **Performance note**: The BFS itself is O(V+E) and fast. On large graphs
/// (80K+ symbols, 100K+ edges), the dominant cost is loading all symbols and
/// typed edges from the database (~500-700ms). This is inherent to the full-
/// graph traversal approach and cannot be reduced without pre-computed caching.
pub fn detect_dead_code(store: &GraphStore) -> anyhow::Result<DeadCodeResult> {
    detect_dead_code_inner(store, DEFAULT_MIN_EDGE_CONFIDENCE, &HashMap::new())
}

/// Like [`detect_dead_code`] but with an explicit minimum edge confidence
/// threshold. Edges with confidence below `min_edge_confidence` are not
/// traversed at all. Symbols reachable only via edges below
/// [`WEAK_EDGE_THRESHOLD`] (0.5) are reported as Medium confidence dead code.
pub fn detect_dead_code_with_confidence(
    store: &GraphStore,
    min_edge_confidence: f32,
) -> anyhow::Result<DeadCodeResult> {
    detect_dead_code_inner(store, min_edge_confidence, &HashMap::new())
}

/// Like [`detect_dead_code`] but also accepts parsed manifest data so that
/// symbols in manifest entry files (`main`, `bin`, `exports`) are treated as
/// entry points.
pub fn detect_dead_code_with_manifests(
    store: &GraphStore,
    manifests: &HashMap<String, ManifestInfo>,
) -> anyhow::Result<DeadCodeResult> {
    detect_dead_code_inner(store, DEFAULT_MIN_EDGE_CONFIDENCE, manifests)
}

/// Core implementation combining confidence-aware BFS with type exclusion,
/// manifest-driven entry points, and dead-class method deduplication.
fn detect_dead_code_inner(
    store: &GraphStore,
    min_edge_confidence: f32,
    manifests: &HashMap<String, ManifestInfo>,
) -> anyhow::Result<DeadCodeResult> {
    // 1. Load all symbols and partition into analysable / excluded.
    let raw_symbols = store
        .list_all_symbols()
        .map_err(|e| anyhow::anyhow!("list_all_symbols: {e}"))?;

    let excluded_count = raw_symbols
        .iter()
        .filter(|s| is_excluded_from_dead_code(s))
        .count();
    let all_symbols: Vec<_> = raw_symbols
        .into_iter()
        .filter(|s| !is_excluded_from_dead_code(s))
        .collect();

    if all_symbols.is_empty() {
        return Ok(DeadCodeResult {
            unreachable_symbols: vec![],
            total_symbols: 0,
            reachable_symbols: 0,
            dead_percentage: 0.0,
            excluded_count,
        });
    }

    // 2. Load the full code graph (symbols + typed edges).
    let typed_edges = store
        .load_typed_edges()
        .map_err(|e| anyhow::anyhow!("load_typed_edges: {e}"))?;

    // Build adjacency list: source -> [(target, confidence)].
    // Also add reverse MEMBER_OF edges (class -> member) so that when BFS
    // reaches a class, its members become reachable too.
    // Additionally, track class -> [member_uid] for dedup in step 6.
    let mut adjacency: HashMap<String, Vec<(String, f32)>> = HashMap::new();
    let mut class_members: HashMap<String, Vec<String>> = HashMap::new();
    for (src, dst, edge_type, confidence) in &typed_edges {
        let conf = *confidence as f32;
        adjacency
            .entry(src.clone())
            .or_default()
            .push((dst.clone(), conf));
        if edge_type == "MEMBER_OF" {
            // MEMBER_OF goes member->class; reverse it so class->member is also traversed.
            adjacency
                .entry(dst.clone())
                .or_default()
                .push((src.clone(), conf));
            // Track class -> [members] for dead-class dedup.
            class_members
                .entry(dst.clone())
                .or_default()
                .push(src.clone());
        }
    }

    // 3. Collect manifest entry file paths (normalized, no leading `./`).
    let manifest_entry_files: HashSet<String> = manifests
        .values()
        .flat_map(|m| m.entry_files.iter())
        .map(|p| p.strip_prefix("./").unwrap_or(p).to_string())
        .collect();

    // 4. Identify entry points (flag + manifest-driven).
    let mut entry_point_uids: Vec<String> = Vec::new();
    for sym in &all_symbols {
        if sym.is_entry_point {
            entry_point_uids.push(sym.uid.clone());
            continue;
        }
        // Manifest-driven: exported symbols in manifest entry files.
        if !manifest_entry_files.is_empty() {
            let normalized = sym.file_path.strip_prefix("./").unwrap_or(&sym.file_path);
            if manifest_entry_files.contains(normalized) {
                entry_point_uids.push(sym.uid.clone());
            }
        }
    }

    // 5. Confidence-aware BFS from all entry points.
    //
    // Two-pass BFS:
    //   - `strong_visited`: symbols reachable via at least one path where
    //     every edge has confidence >= WEAK_EDGE_THRESHOLD.
    //   - `weak_visited`: symbols reachable via edges above min_edge_confidence
    //     but NOT via any fully-strong path.
    //
    // We track the "max minimum confidence along any path" for each node.
    // If max_min >= WEAK_EDGE_THRESHOLD the symbol is strongly reachable;
    // otherwise it's weakly reachable.
    let mut best_path_conf: HashMap<String, f32> = HashMap::new();
    let mut queue: VecDeque<(String, f32)> = VecDeque::new();

    for uid in &entry_point_uids {
        let prev = best_path_conf.entry(uid.clone()).or_insert(0.0_f32);
        if *prev < 1.0 {
            *prev = 1.0; // entry points themselves are fully confident
            queue.push_back((uid.clone(), 1.0));
        }
    }

    while let Some((current, path_conf)) = queue.pop_front() {
        if let Some(targets) = adjacency.get(&current) {
            for (target, edge_conf) in targets {
                // Skip edges below the minimum confidence threshold entirely.
                if *edge_conf < min_edge_confidence {
                    continue;
                }

                // The path confidence is the minimum confidence along
                // the entire path from an entry point to this target.
                let new_path_conf = path_conf.min(*edge_conf);

                let entry = best_path_conf.entry(target.clone()).or_insert(0.0_f32);
                if new_path_conf > *entry {
                    *entry = new_path_conf;
                    queue.push_back((target.clone(), new_path_conf));
                }
            }
        }
    }

    // 6. Collect unreachable symbols with confidence scoring.
    let total_symbols = all_symbols.len();

    // Symbols in best_path_conf with strong path confidence are truly reachable.
    let strong_reachable: HashSet<&String> = best_path_conf
        .iter()
        .filter(|(_, conf)| **conf >= WEAK_EDGE_THRESHOLD)
        .map(|(uid, _)| uid)
        .collect();

    // Symbols reachable only via weak paths.
    let weak_reachable: HashSet<&String> = best_path_conf
        .iter()
        .filter(|(uid, conf)| **conf < WEAK_EDGE_THRESHOLD && !strong_reachable.contains(uid))
        .map(|(uid, _)| uid)
        .collect();

    // Build a lookup: uid -> kind for dead-class dedup.
    let kind_by_uid: HashMap<&str, SymbolKind> = all_symbols
        .iter()
        .map(|s| (s.uid.as_str(), s.kind))
        .collect();

    // Find unreachable class UIDs so we can suppress their members.
    let unreachable_class_uids: HashSet<&str> = all_symbols
        .iter()
        .filter(|s| !strong_reachable.contains(&s.uid) && s.kind == SymbolKind::Class)
        .map(|s| s.uid.as_str())
        .collect();

    // Collect member UIDs of dead classes (to suppress from the unreachable list).
    let suppressed_member_uids: HashSet<String> = unreachable_class_uids
        .iter()
        .flat_map(|cls_uid| class_members.get(*cls_uid).cloned().unwrap_or_default())
        .filter(|member_uid| {
            // Only suppress if the member is actually a Method and is also unreachable.
            kind_by_uid.get(member_uid.as_str()) == Some(&SymbolKind::Method)
                && !strong_reachable.contains(member_uid)
        })
        .collect();

    let mut unreachable_symbols: Vec<UnreachableSymbol> = Vec::new();
    for sym in &all_symbols {
        if strong_reachable.contains(&sym.uid) {
            continue;
        }
        // Suppress methods of dead classes — the class itself is reported.
        if suppressed_member_uids.contains(&sym.uid) {
            continue;
        }

        let visibility_str = sym.visibility.to_string();

        // Symbols reachable only via weak edges are reported as Medium
        // confidence dead code — they might be reachable but the edges
        // are not highly confident.
        let confidence = if weak_reachable.contains(&sym.uid) {
            DeadCodeConfidence::Medium
        } else {
            infer_confidence(&sym.name, &visibility_str, &sym.file_path)
        };

        unreachable_symbols.push(UnreachableSymbol {
            uid: sym.uid.clone(),
            name: sym.name.clone(),
            kind: sym.kind.to_string(),
            file_path: sym.file_path.clone(),
            visibility: visibility_str,
            confidence,
        });
    }

    // Sort by confidence descending, then by file path, then by name.
    unreachable_symbols.sort_by(|a, b| {
        b.confidence
            .cmp(&a.confidence)
            .then_with(|| a.file_path.cmp(&b.file_path))
            .then_with(|| a.name.cmp(&b.name))
    });

    let reachable_symbols = total_symbols - unreachable_symbols.len();

    let dead_percentage = if total_symbols > 0 {
        (unreachable_symbols.len() as f64 / total_symbols as f64) * 100.0
    } else {
        0.0
    };

    Ok(DeadCodeResult {
        unreachable_symbols,
        total_symbols,
        reachable_symbols,
        dead_percentage,
        excluded_count,
    })
}

/// Infer dead-code confidence from visibility and naming conventions.
///
/// - Names starting with `_` or lowercase in Go/Rust-like files -> High
///   (private by convention).
/// - `Inferred` visibility with no private signal -> Medium.
/// - Explicitly public symbols -> Low (could be library API).
fn infer_confidence(name: &str, visibility: &str, file_path: &str) -> DeadCodeConfidence {
    // Explicitly private or internal symbols: high confidence.
    if visibility == "private" || visibility == "internal" || visibility == "protected" {
        return DeadCodeConfidence::High;
    }

    // Naming-convention heuristics for private scope:
    //   - Leading underscore (Python, JS/TS, Dart, Ruby)
    //   - Lowercase first char in Go files (unexported)
    //   - Lowercase first char in Kotlin files (typically local)
    if name.starts_with('_') {
        return DeadCodeConfidence::High;
    }
    let is_go = file_path.ends_with(".go");
    if is_go && name.chars().next().is_some_and(|c| c.is_lowercase()) {
        return DeadCodeConfidence::High;
    }

    // Explicitly public: low confidence (could be library API).
    if visibility == "public" {
        return DeadCodeConfidence::Low;
    }

    // Inferred visibility with no strong private signal: medium.
    DeadCodeConfidence::Medium
}

#[cfg(test)]
mod tests {
    use super::*;
    use nestweaver_schema::{EdgeType, ResolvedEdge, Symbol, SymbolKind, Visibility};
    use nestweaver_store::GraphStore;

    fn make_symbol(uid: &str, name: &str, is_entry: bool) -> Symbol {
        Symbol {
            uid: uid.to_string(),
            name: name.to_string(),
            kind: SymbolKind::Function,
            repo_uid: "repo-1".to_string(),
            file_path: "src/lib.rs".to_string(),
            start_line: 1,
            end_line: 1,
            signature: format!("fn {name}()"),
            summary: None,
            content_hash: "hash".to_string(),
            embedding: None,
            pagerank_score: Some(0.5),
            is_entry_point: is_entry,
            entry_point_kind: if is_entry {
                Some(nestweaver_schema::EntryPointKind::Main)
            } else {
                None
            },
            visibility: Visibility::Inferred,
            type_info: None,
            framework_hint: None,
        }
    }

    fn make_symbol_with_kind(
        uid: &str,
        name: &str,
        kind: SymbolKind,
        file_path: &str,
        is_entry: bool,
    ) -> Symbol {
        Symbol {
            uid: uid.to_string(),
            name: name.to_string(),
            kind,
            repo_uid: "repo-1".to_string(),
            file_path: file_path.to_string(),
            start_line: 1,
            end_line: 1,
            signature: format!("fn {name}()"),
            summary: None,
            content_hash: "hash".to_string(),
            embedding: None,
            pagerank_score: Some(0.5),
            is_entry_point: is_entry,
            entry_point_kind: if is_entry {
                Some(nestweaver_schema::EntryPointKind::Main)
            } else {
                None
            },
            visibility: Visibility::Inferred,
            type_info: None,
            framework_hint: None,
        }
    }

    #[test]
    fn empty_graph_returns_empty_result() {
        let store = GraphStore::in_memory().unwrap();
        let result = detect_dead_code(&store).unwrap();
        assert_eq!(result.total_symbols, 0);
        assert_eq!(result.reachable_symbols, 0);
        assert!(result.unreachable_symbols.is_empty());
        assert_eq!(result.excluded_count, 0);
    }

    #[test]
    fn all_reachable_from_entry_point() {
        let store = GraphStore::in_memory().unwrap();

        // entry -> a -> b
        store
            .insert_symbol(&make_symbol("entry", "main", true))
            .unwrap();
        store
            .insert_symbol(&make_symbol("a", "fn_a", false))
            .unwrap();
        store
            .insert_symbol(&make_symbol("b", "fn_b", false))
            .unwrap();

        store
            .insert_edge(&ResolvedEdge {
                source_uid: "entry".to_string(),
                target_uid: "a".to_string(),
                edge_type: EdgeType::Calls,
                confidence: 0.9,
                link_type: None,
            })
            .unwrap();
        store
            .insert_edge(&ResolvedEdge {
                source_uid: "a".to_string(),
                target_uid: "b".to_string(),
                edge_type: EdgeType::Calls,
                confidence: 0.9,
                link_type: None,
            })
            .unwrap();

        let result = detect_dead_code(&store).unwrap();
        assert_eq!(result.total_symbols, 3);
        assert_eq!(result.reachable_symbols, 3);
        assert!(result.unreachable_symbols.is_empty());
        assert_eq!(result.dead_percentage, 0.0);
    }

    #[test]
    fn detects_unreachable_symbol() {
        let store = GraphStore::in_memory().unwrap();

        // entry -> a, but orphan is disconnected
        store
            .insert_symbol(&make_symbol("entry", "main", true))
            .unwrap();
        store
            .insert_symbol(&make_symbol("a", "fn_a", false))
            .unwrap();
        store
            .insert_symbol(&make_symbol("orphan", "orphan_fn", false))
            .unwrap();

        store
            .insert_edge(&ResolvedEdge {
                source_uid: "entry".to_string(),
                target_uid: "a".to_string(),
                edge_type: EdgeType::Calls,
                confidence: 0.9,
                link_type: None,
            })
            .unwrap();

        let result = detect_dead_code(&store).unwrap();
        assert_eq!(result.total_symbols, 3);
        assert_eq!(result.reachable_symbols, 2);
        assert_eq!(result.unreachable_symbols.len(), 1);
        assert_eq!(result.unreachable_symbols[0].name, "orphan_fn");
    }

    #[test]
    fn follows_imports_and_extends_edges() {
        let store = GraphStore::in_memory().unwrap();

        // entry --imports--> imported --extends--> base
        store
            .insert_symbol(&make_symbol("entry", "main", true))
            .unwrap();
        store
            .insert_symbol(&make_symbol("imported", "Imported", false))
            .unwrap();
        store
            .insert_symbol(&make_symbol("base", "Base", false))
            .unwrap();

        store
            .insert_edge(&ResolvedEdge {
                source_uid: "entry".to_string(),
                target_uid: "imported".to_string(),
                edge_type: EdgeType::Imports,
                confidence: 0.9,
                link_type: None,
            })
            .unwrap();
        store
            .insert_edge(&ResolvedEdge {
                source_uid: "imported".to_string(),
                target_uid: "base".to_string(),
                edge_type: EdgeType::Extends,
                confidence: 0.9,
                link_type: None,
            })
            .unwrap();

        let result = detect_dead_code(&store).unwrap();
        assert_eq!(result.reachable_symbols, 3);
        assert!(result.unreachable_symbols.is_empty());
    }

    #[test]
    fn no_entry_points_marks_everything_unreachable() {
        let store = GraphStore::in_memory().unwrap();

        store
            .insert_symbol(&make_symbol("a", "fn_a", false))
            .unwrap();
        store
            .insert_symbol(&make_symbol("b", "fn_b", false))
            .unwrap();

        store
            .insert_edge(&ResolvedEdge {
                source_uid: "a".to_string(),
                target_uid: "b".to_string(),
                edge_type: EdgeType::Calls,
                confidence: 0.9,
                link_type: None,
            })
            .unwrap();

        let result = detect_dead_code(&store).unwrap();
        assert_eq!(result.total_symbols, 2);
        assert_eq!(result.reachable_symbols, 0);
        assert_eq!(result.unreachable_symbols.len(), 2);
    }

    #[test]
    fn imports_chain_reaches_transitive_deps_and_detects_dead_private() {
        let store = GraphStore::in_memory().unwrap();

        // Build a realistic multi-module graph:
        //   entry (entry point) --IMPORTS--> moduleB_pub
        //   moduleB_pub --CALLS--> moduleC_util
        //   moduleC_dead (private, no incoming edges) <- truly dead
        let mut entry = make_symbol("entry", "App", true);
        entry.file_path = "src/app.tsx".to_string();
        store.insert_symbol(&entry).unwrap();

        let mut module_b = make_symbol("moduleB_pub", "formatDate", false);
        module_b.file_path = "src/utils/date.ts".to_string();
        store.insert_symbol(&module_b).unwrap();

        let mut module_c = make_symbol("moduleC_util", "parseISO", false);
        module_c.file_path = "src/utils/parse.ts".to_string();
        store.insert_symbol(&module_c).unwrap();

        let mut dead_fn = make_symbol("moduleC_dead", "_unusedHelper", false);
        dead_fn.file_path = "src/utils/parse.ts".to_string();
        dead_fn.visibility = Visibility::Private;
        store.insert_symbol(&dead_fn).unwrap();

        // entry --IMPORTS--> moduleB_pub
        store
            .insert_edge(&ResolvedEdge {
                source_uid: "entry".to_string(),
                target_uid: "moduleB_pub".to_string(),
                edge_type: EdgeType::Imports,
                confidence: 0.9,
                link_type: None,
            })
            .unwrap();

        // moduleB_pub --CALLS--> moduleC_util
        store
            .insert_edge(&ResolvedEdge {
                source_uid: "moduleB_pub".to_string(),
                target_uid: "moduleC_util".to_string(),
                edge_type: EdgeType::Calls,
                confidence: 0.9,
                link_type: None,
            })
            .unwrap();

        let result = detect_dead_code(&store).unwrap();

        // entry, moduleB_pub, moduleC_util should all be reachable
        assert_eq!(result.total_symbols, 4);
        assert_eq!(result.reachable_symbols, 3);

        // Only the private unused helper is dead
        assert_eq!(result.unreachable_symbols.len(), 1);
        assert_eq!(result.unreachable_symbols[0].name, "_unusedHelper");
        assert_eq!(
            result.unreachable_symbols[0].confidence,
            DeadCodeConfidence::High
        );
    }

    #[test]
    fn member_of_reverse_traversal_reaches_class_members() {
        let store = GraphStore::in_memory().unwrap();

        // entry --IMPORTS--> MyClass
        // method --MEMBER_OF--> MyClass  (BFS should reverse this to reach method)
        store
            .insert_symbol(&make_symbol("entry", "main", true))
            .unwrap();
        store
            .insert_symbol(&make_symbol("cls", "MyClass", false))
            .unwrap();
        store
            .insert_symbol(&make_symbol("method", "doWork", false))
            .unwrap();

        store
            .insert_edge(&ResolvedEdge {
                source_uid: "entry".to_string(),
                target_uid: "cls".to_string(),
                edge_type: EdgeType::Imports,
                confidence: 0.9,
                link_type: None,
            })
            .unwrap();
        store
            .insert_edge(&ResolvedEdge {
                source_uid: "method".to_string(),
                target_uid: "cls".to_string(),
                edge_type: EdgeType::MemberOf,
                confidence: 0.9,
                link_type: None,
            })
            .unwrap();

        let result = detect_dead_code(&store).unwrap();
        assert_eq!(result.total_symbols, 3);
        assert_eq!(result.reachable_symbols, 3);
        assert!(result.unreachable_symbols.is_empty());
    }

    #[test]
    fn confidence_scoring_private_names() {
        // Leading underscore -> High
        assert_eq!(
            infer_confidence("_helper", "inferred", "src/lib.py"),
            DeadCodeConfidence::High
        );
        // Go lowercase -> High
        assert_eq!(
            infer_confidence("helper", "inferred", "pkg/utils.go"),
            DeadCodeConfidence::High
        );
        // Public -> Low
        assert_eq!(
            infer_confidence("Helper", "public", "src/lib.rs"),
            DeadCodeConfidence::Low
        );
        // Inferred, no private signal -> Medium
        assert_eq!(
            infer_confidence("Helper", "inferred", "src/lib.rs"),
            DeadCodeConfidence::Medium
        );
        // Explicit private -> High
        assert_eq!(
            infer_confidence("Helper", "private", "src/lib.rs"),
            DeadCodeConfidence::High
        );
    }

    // ── Confidence-aware BFS tests ──

    #[test]
    fn low_confidence_edges_are_skipped_below_threshold() {
        let store = GraphStore::in_memory().unwrap();

        // entry --0.2--> weak_target (below default 0.3 threshold)
        store
            .insert_symbol(&make_symbol("entry", "main", true))
            .unwrap();
        store
            .insert_symbol(&make_symbol("weak_target", "weakFn", false))
            .unwrap();

        store
            .insert_edge(&ResolvedEdge {
                source_uid: "entry".to_string(),
                target_uid: "weak_target".to_string(),
                edge_type: EdgeType::Calls,
                confidence: 0.2,
                link_type: None,
            })
            .unwrap();

        let result = detect_dead_code(&store).unwrap();
        // weak_target should NOT be reachable (edge below 0.3)
        assert_eq!(result.unreachable_symbols.len(), 1);
        assert_eq!(result.unreachable_symbols[0].name, "weakFn");
    }

    #[test]
    fn weak_edges_produce_medium_confidence_dead_code() {
        let store = GraphStore::in_memory().unwrap();

        // entry --0.4--> borderline (above 0.3 min, below 0.5 weak threshold)
        store
            .insert_symbol(&make_symbol("entry", "main", true))
            .unwrap();
        store
            .insert_symbol(&make_symbol("borderline", "maybeDead", false))
            .unwrap();

        store
            .insert_edge(&ResolvedEdge {
                source_uid: "entry".to_string(),
                target_uid: "borderline".to_string(),
                edge_type: EdgeType::Calls,
                confidence: 0.4,
                link_type: None,
            })
            .unwrap();

        let result = detect_dead_code(&store).unwrap();
        // borderline should be reported as Medium confidence dead code
        // because it's only reachable via a weak edge (0.4 < 0.5)
        assert_eq!(result.unreachable_symbols.len(), 1);
        assert_eq!(result.unreachable_symbols[0].name, "maybeDead");
        assert_eq!(
            result.unreachable_symbols[0].confidence,
            DeadCodeConfidence::Medium
        );
    }

    #[test]
    fn strong_edges_still_mark_symbols_as_reachable() {
        let store = GraphStore::in_memory().unwrap();

        // entry --0.9--> strong_target (well above both thresholds)
        store
            .insert_symbol(&make_symbol("entry", "main", true))
            .unwrap();
        store
            .insert_symbol(&make_symbol("strong_target", "strongFn", false))
            .unwrap();

        store
            .insert_edge(&ResolvedEdge {
                source_uid: "entry".to_string(),
                target_uid: "strong_target".to_string(),
                edge_type: EdgeType::Calls,
                confidence: 0.9,
                link_type: None,
            })
            .unwrap();

        let result = detect_dead_code(&store).unwrap();
        assert_eq!(result.reachable_symbols, 2);
        assert!(result.unreachable_symbols.is_empty());
    }

    #[test]
    fn custom_min_confidence_threshold() {
        let store = GraphStore::in_memory().unwrap();

        // entry --0.5--> target
        store
            .insert_symbol(&make_symbol("entry", "main", true))
            .unwrap();
        store
            .insert_symbol(&make_symbol("target", "fn_a", false))
            .unwrap();

        store
            .insert_edge(&ResolvedEdge {
                source_uid: "entry".to_string(),
                target_uid: "target".to_string(),
                edge_type: EdgeType::Calls,
                confidence: 0.5,
                link_type: None,
            })
            .unwrap();

        // With min_confidence=0.6, the edge (0.5) should be skipped entirely
        let result = detect_dead_code_with_confidence(&store, 0.6).unwrap();
        assert_eq!(result.unreachable_symbols.len(), 1);
        assert_eq!(result.unreachable_symbols[0].name, "fn_a");

        // With min_confidence=0.3 (default), the edge (0.5) should be traversed
        // and 0.5 >= 0.5 weak threshold, so strongly reachable
        let result = detect_dead_code_with_confidence(&store, 0.3).unwrap();
        assert!(result.unreachable_symbols.is_empty());
    }

    #[test]
    fn mixed_strong_and_weak_paths_uses_best() {
        let store = GraphStore::in_memory().unwrap();

        // entry --0.4--> target (weak path)
        // entry --0.9--> middle --0.8--> target (strong path)
        store
            .insert_symbol(&make_symbol("entry", "main", true))
            .unwrap();
        store
            .insert_symbol(&make_symbol("middle", "helper", false))
            .unwrap();
        store
            .insert_symbol(&make_symbol("target", "fn_a", false))
            .unwrap();

        // Weak direct path
        store
            .insert_edge(&ResolvedEdge {
                source_uid: "entry".to_string(),
                target_uid: "target".to_string(),
                edge_type: EdgeType::Calls,
                confidence: 0.4,
                link_type: None,
            })
            .unwrap();

        // Strong indirect path
        store
            .insert_edge(&ResolvedEdge {
                source_uid: "entry".to_string(),
                target_uid: "middle".to_string(),
                edge_type: EdgeType::Calls,
                confidence: 0.9,
                link_type: None,
            })
            .unwrap();
        store
            .insert_edge(&ResolvedEdge {
                source_uid: "middle".to_string(),
                target_uid: "target".to_string(),
                edge_type: EdgeType::Calls,
                confidence: 0.8,
                link_type: None,
            })
            .unwrap();

        let result = detect_dead_code(&store).unwrap();
        // target should be strongly reachable via the strong path (min 0.8 >= 0.5)
        assert_eq!(result.reachable_symbols, 3);
        assert!(result.unreachable_symbols.is_empty());
    }

    // ── Type exclusion tests ──

    #[test]
    fn type_alias_excluded_from_dead_code() {
        let store = GraphStore::in_memory().unwrap();
        store
            .insert_symbol(&make_symbol("entry", "main", true))
            .unwrap();
        store
            .insert_symbol(&make_symbol_with_kind(
                "alias",
                "MyType",
                SymbolKind::TypeAlias,
                "src/types.ts",
                false,
            ))
            .unwrap();

        let result = detect_dead_code(&store).unwrap();
        assert_eq!(result.excluded_count, 1);
        // TypeAlias should not appear in total_symbols or unreachable.
        assert_eq!(result.total_symbols, 1);
        assert!(result.unreachable_symbols.is_empty());
    }

    #[test]
    fn interface_excluded_from_dead_code() {
        let store = GraphStore::in_memory().unwrap();
        store
            .insert_symbol(&make_symbol("entry", "main", true))
            .unwrap();
        store
            .insert_symbol(&make_symbol_with_kind(
                "iface",
                "IUser",
                SymbolKind::Interface,
                "src/types.ts",
                false,
            ))
            .unwrap();

        let result = detect_dead_code(&store).unwrap();
        assert_eq!(result.excluded_count, 1);
        assert_eq!(result.total_symbols, 1);
    }

    #[test]
    fn property_excluded_from_dead_code() {
        let store = GraphStore::in_memory().unwrap();
        store
            .insert_symbol(&make_symbol("entry", "main", true))
            .unwrap();
        store
            .insert_symbol(&make_symbol_with_kind(
                "prop",
                "name",
                SymbolKind::Property,
                "src/model.ts",
                false,
            ))
            .unwrap();

        let result = detect_dead_code(&store).unwrap();
        assert_eq!(result.excluded_count, 1);
        assert_eq!(result.total_symbols, 1);
    }

    #[test]
    fn d_ts_symbols_excluded_from_dead_code() {
        let store = GraphStore::in_memory().unwrap();
        store
            .insert_symbol(&make_symbol("entry", "main", true))
            .unwrap();
        // A function in a .d.ts file should be excluded.
        store
            .insert_symbol(&make_symbol_with_kind(
                "decl",
                "fetchData",
                SymbolKind::Function,
                "src/api.d.ts",
                false,
            ))
            .unwrap();

        let result = detect_dead_code(&store).unwrap();
        assert_eq!(result.excluded_count, 1);
        assert_eq!(result.total_symbols, 1);
    }

    // ── Dead class method dedup tests ──

    #[test]
    fn dead_class_methods_not_double_counted() {
        let store = GraphStore::in_memory().unwrap();

        store
            .insert_symbol(&make_symbol("entry", "main", true))
            .unwrap();

        // Dead class with two methods.
        store
            .insert_symbol(&make_symbol_with_kind(
                "cls",
                "DeadClass",
                SymbolKind::Class,
                "src/dead.ts",
                false,
            ))
            .unwrap();
        store
            .insert_symbol(&make_symbol_with_kind(
                "m1",
                "methodA",
                SymbolKind::Method,
                "src/dead.ts",
                false,
            ))
            .unwrap();
        store
            .insert_symbol(&make_symbol_with_kind(
                "m2",
                "methodB",
                SymbolKind::Method,
                "src/dead.ts",
                false,
            ))
            .unwrap();

        // m1 --MEMBER_OF--> cls, m2 --MEMBER_OF--> cls
        store
            .insert_edge(&ResolvedEdge {
                source_uid: "m1".to_string(),
                target_uid: "cls".to_string(),
                edge_type: EdgeType::MemberOf,
                confidence: 0.9,
                link_type: None,
            })
            .unwrap();
        store
            .insert_edge(&ResolvedEdge {
                source_uid: "m2".to_string(),
                target_uid: "cls".to_string(),
                edge_type: EdgeType::MemberOf,
                confidence: 0.9,
                link_type: None,
            })
            .unwrap();

        let result = detect_dead_code(&store).unwrap();
        // Only the class should be in unreachable, not its methods.
        assert_eq!(result.unreachable_symbols.len(), 1);
        assert_eq!(result.unreachable_symbols[0].name, "DeadClass");
        assert_eq!(result.unreachable_symbols[0].kind, "Class");
    }

    #[test]
    fn reachable_class_methods_not_suppressed() {
        let store = GraphStore::in_memory().unwrap();

        // entry -> cls (reachable class), method is MEMBER_OF cls
        store
            .insert_symbol(&make_symbol("entry", "main", true))
            .unwrap();
        store
            .insert_symbol(&make_symbol_with_kind(
                "cls",
                "LiveClass",
                SymbolKind::Class,
                "src/live.ts",
                false,
            ))
            .unwrap();
        store
            .insert_symbol(&make_symbol_with_kind(
                "m1",
                "methodA",
                SymbolKind::Method,
                "src/live.ts",
                false,
            ))
            .unwrap();

        store
            .insert_edge(&ResolvedEdge {
                source_uid: "entry".to_string(),
                target_uid: "cls".to_string(),
                edge_type: EdgeType::Calls,
                confidence: 0.9,
                link_type: None,
            })
            .unwrap();
        store
            .insert_edge(&ResolvedEdge {
                source_uid: "m1".to_string(),
                target_uid: "cls".to_string(),
                edge_type: EdgeType::MemberOf,
                confidence: 0.9,
                link_type: None,
            })
            .unwrap();

        let result = detect_dead_code(&store).unwrap();
        // Class is reachable, BFS reaches method via reverse MEMBER_OF.
        assert!(result.unreachable_symbols.is_empty());
    }

    // ── Manifest-driven entry point tests ──

    #[test]
    fn manifest_entry_files_mark_symbols_as_entry_points() {
        let store = GraphStore::in_memory().unwrap();

        // No explicit entry point, but the symbol's file is a manifest entry.
        store
            .insert_symbol(&make_symbol_with_kind(
                "lib",
                "libMain",
                SymbolKind::Function,
                "src/index.ts",
                false,
            ))
            .unwrap();
        store
            .insert_symbol(&make_symbol_with_kind(
                "orphan",
                "orphanFn",
                SymbolKind::Function,
                "src/utils.ts",
                false,
            ))
            .unwrap();

        let mut manifests = HashMap::new();
        manifests.insert(
            "repo-1".to_string(),
            ManifestInfo {
                package_name: Some("my-pkg".to_string()),
                dependencies: vec![],
                entry_files: vec!["./src/index.ts".to_string()],
            },
        );

        let result = detect_dead_code_with_manifests(&store, &manifests).unwrap();
        assert_eq!(result.total_symbols, 2);
        // libMain should be reachable (manifest entry), orphanFn should not.
        assert_eq!(result.reachable_symbols, 1);
        assert_eq!(result.unreachable_symbols.len(), 1);
        assert_eq!(result.unreachable_symbols[0].name, "orphanFn");
    }

    #[test]
    fn manifest_entry_file_without_leading_dot_slash() {
        let store = GraphStore::in_memory().unwrap();

        store
            .insert_symbol(&make_symbol_with_kind(
                "bin",
                "cliMain",
                SymbolKind::Function,
                "bin/cli.js",
                false,
            ))
            .unwrap();

        let mut manifests = HashMap::new();
        manifests.insert(
            "repo-1".to_string(),
            ManifestInfo {
                package_name: Some("my-cli".to_string()),
                dependencies: vec![],
                entry_files: vec!["bin/cli.js".to_string()],
            },
        );

        let result = detect_dead_code_with_manifests(&store, &manifests).unwrap();
        assert_eq!(result.reachable_symbols, 1);
        assert!(result.unreachable_symbols.is_empty());
    }
}
