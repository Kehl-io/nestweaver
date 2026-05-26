import Graph from "graphology";
import type { ImpactNode } from "../../../api/types";
import { EDGE_COLORS, nodeSize } from "./graphColors";

export function buildGraphFromImpact(
  targetUid: string,
  targetName: string,
  nodes: ImpactNode[],
): Graph {
  const graph = new Graph({ type: "directed", multi: true });
  const maxDepth = Math.max(...nodes.map((n) => n.depth), 1);

  graph.addNode(targetUid, {
    label: targetName,
    x: 0,
    y: 0,
    size: 20,
    color: "#EF4444",
    kind: "target",
    depth: 0,
    forceLabel: true,
  });

  for (const node of nodes) {
    if (graph.hasNode(node.uid)) continue;

    const angle = Math.random() * Math.PI * 2;
    const radius = node.depth * 150;
    const frac = node.depth / maxDepth;
    const r = Math.round(239 - frac * 180);
    const g = Math.round(68 + frac * 187);
    const color = `#${r.toString(16).padStart(2, "0")}${g.toString(16).padStart(2, "0")}44`;

    graph.addNode(node.uid, {
      label: node.name,
      x: Math.cos(angle) * radius,
      y: Math.sin(angle) * radius,
      size: 12,
      color,
      kind: "impact",
      depth: node.depth,
      edgeType: node.edge_type,
      confidence: node.confidence,
      filePath: node.file_path,
    });

    const edgeColor = EDGE_COLORS[node.edge_type] || "#9CA3AF";
    graph.addEdge(node.uid, targetUid, {
      type: "arrow",
      size: Math.max(1, node.confidence * 3),
      color: edgeColor,
      label: node.edge_type,
      edgeType: node.edge_type,
      confidence: node.confidence,
    });
  }

  // Second pass: update node sizes based on actual degree
  graph.setNodeAttribute(targetUid, "size", nodeSize(graph.degree(targetUid), 0.01));
  graph.forEachNode((nodeId) => {
    if (nodeId === targetUid) return;
    graph.setNodeAttribute(nodeId, "size", nodeSize(graph.degree(nodeId), 0.001));
  });

  return graph;
}
