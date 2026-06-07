//! Parser for Mermaid diagram syntax embedded in markdown code blocks.
//!
//! Extracts nodes and edges from `flowchart` and `graph` diagrams.
//! Sequence diagrams, class diagrams, and other types are detected but
//! not deeply parsed (they return the diagram type with empty nodes/edges).

use regex::Regex;
use std::collections::HashSet;
use std::sync::LazyLock;

#[derive(Debug, Clone)]
pub struct MermaidNode {
    pub id: String,
    pub label: Option<String>,
}

#[derive(Debug, Clone)]
pub struct MermaidEdge {
    pub from: String,
    pub to: String,
    pub label: Option<String>,
}

#[derive(Debug, Clone)]
pub struct MermaidDiagram {
    pub diagram_type: String,
    pub nodes: Vec<MermaidNode>,
    pub edges: Vec<MermaidEdge>,
}

static RE_DIAGRAM_TYPE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)^(flowchart|graph|sequenceDiagram|classDiagram|stateDiagram|erDiagram|gantt|pie)\b",
    )
    .unwrap()
});

// Matches: A[Label] --> B[Label]  or  A --> B  or  A -- text --> B
// Also: A --- B, A ==> B, A -.-> B
static RE_EDGE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(\w+)(?:\[([^\]]*)\]|\(([^)]*)\)|\{([^}]*)\})?\s*(?:-->|---|==>|-\.->|--\s*([^-|>]+?)\s*-->)\s*(\w+)(?:\[([^\]]*)\]|\(([^)]*)\)|\{([^}]*)\})?"
    )
    .unwrap()
});

// Matches standalone node declarations: A[Label] or A(Label) or A{Label}
static RE_NODE_DECL: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\s*(\w+)\[([^\]]*)\]\s*$").unwrap());

pub fn parse_mermaid(source: &str) -> Option<MermaidDiagram> {
    let first_line = source.lines().next()?.trim();
    let diagram_type = RE_DIAGRAM_TYPE
        .captures(first_line)?
        .get(1)?
        .as_str()
        .to_string();

    let mut nodes: Vec<MermaidNode> = Vec::new();
    let mut edges: Vec<MermaidEdge> = Vec::new();
    let mut seen_ids: HashSet<String> = HashSet::new();

    for line in source.lines().skip(1) {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with("%%") || trimmed == "end" {
            continue;
        }

        // Try edge pattern first
        if let Some(caps) = RE_EDGE.captures(trimmed) {
            let from_id = caps
                .get(1)
                .map(|m| m.as_str().to_string())
                .unwrap_or_default();
            let from_label = caps
                .get(2)
                .or(caps.get(3))
                .or(caps.get(4))
                .map(|m| m.as_str().to_string());
            let edge_label = caps
                .get(5)
                .map(|m| m.as_str().trim().to_string())
                .filter(|s| !s.is_empty());
            let to_id = caps
                .get(6)
                .map(|m| m.as_str().to_string())
                .unwrap_or_default();
            let to_label = caps
                .get(7)
                .or(caps.get(8))
                .or(caps.get(9))
                .map(|m| m.as_str().to_string());

            if !from_id.is_empty() && !to_id.is_empty() {
                if seen_ids.insert(from_id.clone()) {
                    nodes.push(MermaidNode {
                        id: from_id.clone(),
                        label: from_label,
                    });
                }
                if seen_ids.insert(to_id.clone()) {
                    nodes.push(MermaidNode {
                        id: to_id.clone(),
                        label: to_label,
                    });
                }
                edges.push(MermaidEdge {
                    from: from_id,
                    to: to_id,
                    label: edge_label,
                });
            }
            continue;
        }

        // Try standalone node declaration
        if let Some(caps) = RE_NODE_DECL.captures(trimmed) {
            let id = caps
                .get(1)
                .map(|m| m.as_str().to_string())
                .unwrap_or_default();
            let label = caps.get(2).map(|m| m.as_str().to_string());
            if !id.is_empty() && seen_ids.insert(id.clone()) {
                nodes.push(MermaidNode { id, label });
            }
        }
    }

    Some(MermaidDiagram {
        diagram_type,
        nodes,
        edges,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_flowchart() {
        let src = "flowchart TD\n    A[Start] --> B[Process]\n    B --> C[End]\n";
        let diagram = parse_mermaid(src).unwrap();
        assert_eq!(diagram.diagram_type, "flowchart");
        assert_eq!(diagram.nodes.len(), 3);
        assert_eq!(diagram.edges.len(), 2);
        assert_eq!(diagram.edges[0].from, "A");
        assert_eq!(diagram.edges[0].to, "B");
    }

    #[test]
    fn parses_graph_lr() {
        let src = "graph LR\n    api[API Gateway] --> svc[Service]\n    svc --> db[Database]\n";
        let diagram = parse_mermaid(src).unwrap();
        assert_eq!(diagram.diagram_type, "graph");
        assert_eq!(diagram.nodes.len(), 3);
        assert_eq!(diagram.nodes[0].label.as_deref(), Some("API Gateway"));
    }

    #[test]
    fn parses_labeled_edges() {
        let src = "flowchart TD\n    A -- sends request --> B\n";
        let diagram = parse_mermaid(src).unwrap();
        assert_eq!(diagram.edges.len(), 1);
        assert_eq!(diagram.edges[0].label.as_deref(), Some("sends request"));
    }

    #[test]
    fn deduplicates_nodes() {
        let src = "flowchart TD\n    A --> B\n    B --> C\n    A --> C\n";
        let diagram = parse_mermaid(src).unwrap();
        assert_eq!(diagram.nodes.len(), 3);
        assert_eq!(diagram.edges.len(), 3);
    }

    #[test]
    fn skips_comments() {
        let src = "flowchart TD\n    %% This is a comment\n    A --> B\n";
        let diagram = parse_mermaid(src).unwrap();
        assert_eq!(diagram.nodes.len(), 2);
        assert_eq!(diagram.edges.len(), 1);
    }

    #[test]
    fn detects_sequence_diagram() {
        let src = "sequenceDiagram\n    Alice->>Bob: Hello\n";
        let diagram = parse_mermaid(src).unwrap();
        assert_eq!(diagram.diagram_type, "sequenceDiagram");
    }

    #[test]
    fn non_mermaid_returns_none() {
        assert!(parse_mermaid("just some text").is_none());
    }
}
