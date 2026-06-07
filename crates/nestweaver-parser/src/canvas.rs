//! Parser for Obsidian Canvas files (`.canvas`).
//!
//! Canvas files are JSON with `{nodes: [...], edges: [...]}`. Each node has an
//! `id`, `type` (file | text | link | group), and type-specific fields. Edges
//! connect node IDs with optional labels.

use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct CanvasFile {
    #[serde(default)]
    pub nodes: Vec<CanvasNode>,
    #[serde(default)]
    pub edges: Vec<CanvasEdge>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CanvasNode {
    pub id: String,
    #[serde(rename = "type")]
    pub node_type: String,
    #[serde(default)]
    pub file: Option<String>,
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default)]
    pub x: f64,
    #[serde(default)]
    pub y: f64,
    #[serde(default)]
    pub width: f64,
    #[serde(default)]
    pub height: f64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CanvasEdge {
    pub id: String,
    #[serde(rename = "fromNode")]
    pub from_node: String,
    #[serde(rename = "toNode")]
    pub to_node: String,
    #[serde(default)]
    pub label: Option<String>,
}

pub fn parse_canvas(source: &str) -> Result<CanvasFile, serde_json::Error> {
    serde_json::from_str(source)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_minimal_canvas() {
        let json = r#"{"nodes":[],"edges":[]}"#;
        let canvas = parse_canvas(json).unwrap();
        assert!(canvas.nodes.is_empty());
        assert!(canvas.edges.is_empty());
    }

    #[test]
    fn parses_canvas_with_file_nodes() {
        let json = r#"{
            "nodes": [
                {"id": "1", "type": "file", "file": "notes/architecture.md", "x": 0, "y": 0, "width": 400, "height": 300},
                {"id": "2", "type": "text", "text": "Design decisions", "x": 500, "y": 0, "width": 300, "height": 200}
            ],
            "edges": [
                {"id": "e1", "fromNode": "1", "toNode": "2"}
            ]
        }"#;
        let canvas = parse_canvas(json).unwrap();
        assert_eq!(canvas.nodes.len(), 2);
        assert_eq!(canvas.edges.len(), 1);
        assert_eq!(canvas.nodes[0].node_type, "file");
        assert_eq!(
            canvas.nodes[0].file.as_deref(),
            Some("notes/architecture.md")
        );
        assert_eq!(canvas.nodes[1].node_type, "text");
        assert_eq!(
            canvas.nodes[1].text.as_deref(),
            Some("Design decisions")
        );
    }

    #[test]
    fn parses_canvas_with_labeled_edges() {
        let json = r#"{
            "nodes": [
                {"id": "a", "type": "file", "file": "api.md", "x": 0, "y": 0, "width": 100, "height": 100},
                {"id": "b", "type": "file", "file": "db.md", "x": 200, "y": 0, "width": 100, "height": 100}
            ],
            "edges": [
                {"id": "e1", "fromNode": "a", "toNode": "b", "label": "queries"}
            ]
        }"#;
        let canvas = parse_canvas(json).unwrap();
        assert_eq!(canvas.edges[0].label.as_deref(), Some("queries"));
        assert_eq!(canvas.edges[0].from_node, "a");
        assert_eq!(canvas.edges[0].to_node, "b");
    }

    #[test]
    fn parses_canvas_with_groups() {
        let json = r#"{
            "nodes": [
                {"id": "g1", "type": "group", "label": "Backend", "x": 0, "y": 0, "width": 800, "height": 600},
                {"id": "n1", "type": "file", "file": "server.md", "x": 50, "y": 50, "width": 200, "height": 150}
            ],
            "edges": []
        }"#;
        let canvas = parse_canvas(json).unwrap();
        assert_eq!(canvas.nodes[0].node_type, "group");
        assert_eq!(canvas.nodes[0].label.as_deref(), Some("Backend"));
    }

    #[test]
    fn handles_malformed_json() {
        let result = parse_canvas("not json");
        assert!(result.is_err());
    }
}
