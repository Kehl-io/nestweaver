//! Dead code detection via entry point reachability analysis.
//!
//! Walks forward from every entry point in the graph following
//! CALLS, IMPORTS, EXTENDS, IMPLEMENTS, and MEMBER_OF edges. Any
//! symbol not reached is potentially dead. Confidence scoring
//! accounts for visibility: private unreachable symbols are
//! high-confidence dead code; public ones could be library API.

use std::collections::{HashSet, VecDeque};

use nestweaver_store::GraphStore;
use serde::Serialize;

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
}

/// Detect potentially dead code by walking forward from all entry points.
///
/// Algorithm:
/// 1. Collect every Symbol with `is_entry_point == true`.
/// 2. BFS forward from entry points following reachability edges.
/// 3. Any symbol not in the visited set is reported as unreachable.
/// 4. Confidence is scored based on inferred visibility heuristics.
pub fn detect_dead_code(store: &GraphStore) -> anyhow::Result<DeadCodeResult> {
    // 1. Load all symbols.
    let all_symbols = store
        .list_all_symbols()
        .map_err(|e| anyhow::anyhow!("list_all_symbols: {e}"))?;

    if all_symbols.is_empty() {
        return Ok(DeadCodeResult {
            unreachable_symbols: vec![],
            total_symbols: 0,
            reachable_symbols: 0,
            dead_percentage: 0.0,
        });
    }

    // 2. Load the full code graph (symbols + typed edges).
    let typed_edges = store
        .load_typed_edges()
        .map_err(|e| anyhow::anyhow!("load_typed_edges: {e}"))?;

    // Build adjacency list: source -> [target] (forward direction).
    // Also add reverse MEMBER_OF edges (class -> member) so that when BFS
    // reaches a class, its members become reachable too.
    let mut adjacency: std::collections::HashMap<String, Vec<String>> =
        std::collections::HashMap::new();
    for (src, dst, edge_type, _confidence) in &typed_edges {
        adjacency.entry(src.clone()).or_default().push(dst.clone());
        if edge_type == "MEMBER_OF" {
            // MEMBER_OF goes member→class; reverse it so class→member is also traversed.
            adjacency.entry(dst.clone()).or_default().push(src.clone());
        }
    }

    // 3. Identify entry points.
    let entry_point_uids: Vec<String> = all_symbols
        .iter()
        .filter(|s| s.is_entry_point)
        .map(|s| s.uid.clone())
        .collect();

    // 4. BFS from all entry points.
    let mut visited: HashSet<String> = HashSet::new();
    let mut queue: VecDeque<String> = VecDeque::new();

    for uid in &entry_point_uids {
        if visited.insert(uid.clone()) {
            queue.push_back(uid.clone());
        }
    }

    while let Some(current) = queue.pop_front() {
        if let Some(targets) = adjacency.get(&current) {
            for target in targets {
                if visited.insert(target.clone()) {
                    queue.push_back(target.clone());
                }
            }
        }
    }

    // 5. Collect unreachable symbols with confidence scoring.
    let total_symbols = all_symbols.len();
    let reachable_symbols = visited.len();

    let mut unreachable_symbols: Vec<UnreachableSymbol> = Vec::new();
    for sym in &all_symbols {
        if visited.contains(&sym.uid) {
            continue;
        }

        let visibility_str = sym.visibility.to_string();
        let confidence = infer_confidence(&sym.name, &visibility_str, &sym.file_path);

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
}
