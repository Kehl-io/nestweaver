use crate::graph::InMemoryGraph;

pub struct SearchResult {
    pub uid: String,
    pub name: String,
    pub kind: String,
    pub score: f64,
}

/// Case-insensitive substring search over node names and file paths.
/// Returns matches sorted by relevance (exact match > prefix > substring).
pub fn search(graph: &InMemoryGraph, query: &str, limit: usize) -> Vec<SearchResult> {
    if query.is_empty() {
        return vec![];
    }

    let query_lower = query.to_lowercase();
    let mut results: Vec<SearchResult> = Vec::new();

    for (i, meta) in graph.nodes.iter().enumerate() {
        let name_lower = meta.name.to_lowercase();

        let score = if name_lower == query_lower {
            1.0 // exact match
        } else if name_lower.starts_with(&query_lower) {
            0.8 // prefix match
        } else if name_lower.contains(&query_lower) {
            0.5 // substring match
        } else if meta
            .file_path
            .as_ref()
            .is_some_and(|p| p.to_lowercase().contains(&query_lower))
        {
            0.3 // file path match
        } else {
            continue;
        };

        // Boost by PageRank if available
        let pr_boost = meta.pagerank_score.unwrap_or(0.0).min(1.0);
        let final_score = score + pr_boost * 0.1;

        results.push(SearchResult {
            uid: graph.uids[i].clone(),
            name: meta.name.clone(),
            kind: meta.kind.clone(),
            score: final_score,
        });
    }

    results.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    results.truncate(limit);
    results
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::{InMemoryGraph, NodeMeta};

    fn make_node(name: &str, kind: &str, file_path: Option<&str>) -> NodeMeta {
        NodeMeta {
            name: name.to_string(),
            kind: kind.to_string(),
            file_path: file_path.map(str::to_string),
            pagerank_score: None,
            is_entry_point: false,
        }
    }

    fn make_graph(nodes: Vec<NodeMeta>) -> InMemoryGraph {
        let uids: Vec<String> = (0..nodes.len()).map(|i| format!("uid-{i}")).collect();
        InMemoryGraph {
            uids,
            nodes,
            edges: vec![],
            generation: 0,
        }
    }

    #[test]
    fn empty_query_returns_empty() {
        let graph = make_graph(vec![make_node("foo", "function", None)]);
        let results = search(&graph, "", 10);
        assert!(results.is_empty());
    }

    #[test]
    fn exact_match_scores_highest() {
        let graph = make_graph(vec![
            make_node("greet", "function", None),
            make_node("greetUser", "function", None),
            make_node("sayGreeting", "function", None),
        ]);
        let results = search(&graph, "greet", 10);
        assert!(!results.is_empty());
        assert_eq!(results[0].name, "greet");
        // Exact match score is 1.0, prefix is 0.8, substring is 0.5
        assert!(results[0].score >= 1.0);
    }

    #[test]
    fn prefix_match_scores_second() {
        let graph = make_graph(vec![
            make_node("greetUser", "function", None),
            make_node("sayGreeting", "function", None),
        ]);
        let results = search(&graph, "greet", 10);
        assert_eq!(results.len(), 2);
        // prefix "greetUser" should beat substring "sayGreeting"
        assert_eq!(results[0].name, "greetUser");
        assert!(results[0].score > results[1].score);
    }

    #[test]
    fn case_insensitive_matching() {
        let graph = make_graph(vec![
            make_node("FetchUser", "function", None),
            make_node("fetchData", "function", None),
        ]);
        let results = search(&graph, "FETCH", 10);
        assert_eq!(results.len(), 2);
        let names: Vec<&str> = results.iter().map(|r| r.name.as_str()).collect();
        assert!(names.contains(&"FetchUser"));
        assert!(names.contains(&"fetchData"));
    }

    #[test]
    fn limit_is_respected() {
        let nodes: Vec<NodeMeta> = (0..20)
            .map(|i| make_node(&format!("func{i}"), "function", None))
            .collect();
        let graph = make_graph(nodes);
        let results = search(&graph, "func", 5);
        assert_eq!(results.len(), 5);
    }

    #[test]
    fn file_path_match_included() {
        let graph = make_graph(vec![
            make_node("myHandler", "function", Some("src/routes/users.rs")),
            make_node("otherFn", "function", None),
        ]);
        // "users" only matches via file path
        let results = search(&graph, "users", 10);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].name, "myHandler");
        assert!((results[0].score - 0.3).abs() < 1e-9);
    }

    #[test]
    fn no_match_returns_empty() {
        let graph = make_graph(vec![make_node("alpha", "function", None)]);
        let results = search(&graph, "zzz", 10);
        assert!(results.is_empty());
    }
}
