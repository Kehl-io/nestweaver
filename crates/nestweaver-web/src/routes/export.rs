use std::sync::Arc;

use axum::Json;
use axum::extract::State;
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};

use crate::error::ApiError;
use crate::state::AppState;

#[derive(Deserialize)]
pub struct GraphSnapshot {
    nodes: Vec<SnapshotNode>,
    edges: Vec<SnapshotEdge>,
    width: u32,
    height: u32,
    background: String,
    #[serde(default)]
    legend: bool,
}

#[derive(Deserialize, Serialize)]
pub struct SnapshotNode {
    uid: String,
    x: f64,
    y: f64,
    size: f64,
    color: String,
    label: String,
}

#[derive(Deserialize, Serialize)]
pub struct SnapshotEdge {
    source: String,
    target: String,
    color: String,
    #[serde(default = "default_thickness")]
    thickness: f64,
}

fn default_thickness() -> f64 {
    1.0
}

fn build_svg(snapshot: &GraphSnapshot) -> String {
    let mut svg = format!(
        r#"<svg xmlns="http://www.w3.org/2000/svg" width="{}" height="{}" viewBox="0 0 {} {}">"#,
        snapshot.width, snapshot.height, snapshot.width, snapshot.height,
    );
    svg.push('\n');

    // Background
    svg.push_str(&format!(
        r#"  <rect width="{}" height="{}" fill="{}"/>"#,
        snapshot.width, snapshot.height, snapshot.background,
    ));
    svg.push('\n');

    // Edges
    for edge in &snapshot.edges {
        let src = snapshot.nodes.iter().find(|n| n.uid == edge.source);
        let tgt = snapshot.nodes.iter().find(|n| n.uid == edge.target);
        if let (Some(s), Some(t)) = (src, tgt) {
            svg.push_str(&format!(
                r#"  <line x1="{}" y1="{}" x2="{}" y2="{}" stroke="{}" stroke-width="{}"/>"#,
                s.x, s.y, t.x, t.y, edge.color, edge.thickness,
            ));
            svg.push('\n');
        }
    }

    // Nodes
    for node in &snapshot.nodes {
        svg.push_str(&format!(
            r#"  <circle cx="{}" cy="{}" r="{}" fill="{}"/>"#,
            node.x, node.y, node.size, node.color,
        ));
        svg.push('\n');
        svg.push_str(&format!(
            r##"  <text x="{}" y="{}" font-size="{}" fill="#333" text-anchor="middle" dy=".35em">{}</text>"##,
            node.x,
            node.y,
            (node.size * 0.8).max(8.0),
            html_escape(&node.label),
        ));
        svg.push('\n');
    }

    // Legend
    if snapshot.legend {
        svg.push_str(
            r##"  <text x="10" y="20" font-size="12" fill="#666">NestWeaver Graph Export</text>"##,
        );
        svg.push('\n');
    }

    svg.push_str("</svg>");
    svg
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

pub async fn export_svg(
    State(_state): State<Arc<AppState>>,
    Json(snapshot): Json<GraphSnapshot>,
) -> Result<Response, ApiError> {
    let svg = build_svg(&snapshot);
    Ok(([(axum::http::header::CONTENT_TYPE, "image/svg+xml")], svg).into_response())
}

pub async fn export_png(
    State(_state): State<Arc<AppState>>,
    Json(_snapshot): Json<GraphSnapshot>,
) -> Result<Response, ApiError> {
    Ok((
        axum::http::StatusCode::NOT_IMPLEMENTED,
        axum::Json(serde_json::json!({
            "error": "PNG export requires resvg (not yet enabled)"
        })),
    )
        .into_response())
}

pub async fn export_html(
    State(_state): State<Arc<AppState>>,
    Json(snapshot): Json<GraphSnapshot>,
) -> Result<Response, ApiError> {
    let nodes_json = serde_json::to_string(&snapshot.nodes)
        .map_err(|e| ApiError::internal(format!("failed to serialize nodes: {e}")))?;
    let edges_json = serde_json::to_string(&snapshot.edges)
        .map_err(|e| ApiError::internal(format!("failed to serialize edges: {e}")))?;

    let html = format!(
        r#"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>NestWeaver Graph Export</title>
<style>
  * {{ margin: 0; padding: 0; box-sizing: border-box; }}
  body {{ background: {bg}; overflow: hidden; font-family: system-ui, sans-serif; }}
  canvas {{ display: block; cursor: grab; }}
  canvas:active {{ cursor: grabbing; }}
  #info {{ position: fixed; bottom: 8px; right: 12px; color: #888; font-size: 12px; }}
</style>
</head>
<body>
<canvas id="c"></canvas>
<div id="info">Scroll to zoom &middot; Drag to pan</div>
<script>
const nodes = {nodes_json};
const edges = {edges_json};
const canvas = document.getElementById('c');
const ctx = canvas.getContext('2d');

let scale = 1, offsetX = 0, offsetY = 0;
let dragging = false, lastX = 0, lastY = 0;

function resize() {{
  canvas.width = window.innerWidth;
  canvas.height = window.innerHeight;
  draw();
}}
window.addEventListener('resize', resize);

function draw() {{
  ctx.clearRect(0, 0, canvas.width, canvas.height);
  ctx.save();
  ctx.translate(offsetX, offsetY);
  ctx.scale(scale, scale);

  // edges
  for (const e of edges) {{
    const src = nodes.find(n => n.uid === e.source);
    const tgt = nodes.find(n => n.uid === e.target);
    if (src && tgt) {{
      ctx.beginPath();
      ctx.moveTo(src.x, src.y);
      ctx.lineTo(tgt.x, tgt.y);
      ctx.strokeStyle = e.color;
      ctx.lineWidth = e.thickness || 1;
      ctx.stroke();
    }}
  }}

  // nodes
  for (const n of nodes) {{
    ctx.beginPath();
    ctx.arc(n.x, n.y, n.size, 0, Math.PI * 2);
    ctx.fillStyle = n.color;
    ctx.fill();

    ctx.fillStyle = '#333';
    ctx.font = Math.max(8, n.size * 0.8) + 'px system-ui';
    ctx.textAlign = 'center';
    ctx.textBaseline = 'middle';
    ctx.fillText(n.label, n.x, n.y);
  }}

  ctx.restore();
}}

canvas.addEventListener('wheel', e => {{
  e.preventDefault();
  const factor = e.deltaY < 0 ? 1.1 : 0.9;
  const rect = canvas.getBoundingClientRect();
  const mx = e.clientX - rect.left;
  const my = e.clientY - rect.top;
  offsetX = mx - (mx - offsetX) * factor;
  offsetY = my - (my - offsetY) * factor;
  scale *= factor;
  draw();
}}, {{ passive: false }});

canvas.addEventListener('mousedown', e => {{
  dragging = true;
  lastX = e.clientX;
  lastY = e.clientY;
}});
canvas.addEventListener('mousemove', e => {{
  if (!dragging) return;
  offsetX += e.clientX - lastX;
  offsetY += e.clientY - lastY;
  lastX = e.clientX;
  lastY = e.clientY;
  draw();
}});
canvas.addEventListener('mouseup', () => dragging = false);
canvas.addEventListener('mouseleave', () => dragging = false);

resize();
</script>
</body>
</html>"#,
        bg = snapshot.background,
        nodes_json = nodes_json,
        edges_json = edges_json,
    );

    Ok((
        [(axum::http::header::CONTENT_TYPE, "text/html; charset=utf-8")],
        html,
    )
        .into_response())
}
