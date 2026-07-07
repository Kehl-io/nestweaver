import Graph from "graphology";
import type {
  ImpactLensNode,
  ImpactLensResponse,
} from "../../../api/impactLens";
import { EDGE_COLORS, nodeSize } from "./graphColors";

function impactColor(node: ImpactLensNode, maxLayer: number): string {
  if (node.role === "target") return "#D13444";
  const frac = maxLayer <= 0 ? 0 : node.layer / maxLayer;
  const r = Math.round(14 + frac * 170);
  const g = Math.round(134 - frac * 42);
  const b = Math.round(196 - frac * 112);
  return `#${r.toString(16).padStart(2, "0")}${g.toString(16).padStart(2, "0")}${b.toString(16).padStart(2, "0")}`;
}

function deterministicLayerPositions(nodes: ImpactLensNode[]): Map<string, { x: number; y: number }> {
  const byLayer = new Map<number, ImpactLensNode[]>();
  for (const node of nodes) {
    const layerNodes = byLayer.get(node.layer) ?? [];
    layerNodes.push(node);
    byLayer.set(node.layer, layerNodes);
  }

  const positions = new Map<string, { x: number; y: number }>();
  for (const [layer, layerNodes] of byLayer) {
    layerNodes.sort((left, right) =>
      left.impact_score === right.impact_score
        ? left.uid.localeCompare(right.uid)
        : right.impact_score - left.impact_score,
    );
    const total = layerNodes.length;
    layerNodes.forEach((node, index) => {
      positions.set(node.uid, {
        x: (index - (total - 1) / 2) * 160,
        y: layer * 180,
      });
    });
  }
  return positions;
}

export function buildGraphFromImpact(result: ImpactLensResponse): Graph {
  const graph = new Graph({ type: "directed", multi: true });
  const dedupedNodes = new Map<string, ImpactLensNode>();
  if (result.target) {
    dedupedNodes.set(result.target.uid, result.target);
  }
  for (const node of result.nodes) {
    dedupedNodes.set(node.uid, node);
  }

  const nodes = [...dedupedNodes.values()];
  const maxLayer = Math.max(...nodes.map((node) => node.layer), 1);
  const positions = deterministicLayerPositions(nodes);
  graph.setAttribute("impactTarget", result.target?.uid ?? null);
  graph.setAttribute("impactStates", result.states);
  graph.setAttribute("affectedTests", result.affected_tests);
  graph.setAttribute("sceneMetadata", result._meta);

  for (const node of nodes) {
    const position = positions.get(node.uid) ?? { x: 0, y: 0 };

    graph.addNode(node.uid, {
      label: node.name,
      x: position.x,
      y: position.y,
      size: node.role === "target" ? 20 : 12,
      color: impactColor(node, maxLayer),
      kind: node.role === "target" ? "target" : "impact",
      depth: node.layer,
      layer: node.layer,
      edgeType: node.edge_type ?? undefined,
      confidence: node.confidence,
      impactScore: node.impact_score,
      filePath: node.file_path,
      startLine: node.start_line,
      sourceUrl: node.source?.url,
      forceLabel: node.role === "target",
    });
  }

  for (const edge of result.edges) {
    if (!graph.hasNode(edge.source) || !graph.hasNode(edge.target)) continue;
    const edgeColor = EDGE_COLORS[edge.edge_type.toLowerCase()] || "#6b7280";
    graph.addEdge(edge.source, edge.target, {
      type: "arrow",
      size: Math.max(1, edge.confidence * 3),
      color: edgeColor,
      label: edge.edge_type,
      edgeType: edge.edge_type,
      confidence: edge.confidence,
      sourceLayer: edge.source_layer,
      targetLayer: edge.target_layer,
    });
  }

  // Second pass: update node sizes based on actual degree
  if (result.target && graph.hasNode(result.target.uid)) {
    graph.setNodeAttribute(
      result.target.uid,
      "size",
      nodeSize(graph.degree(result.target.uid), 0.01),
    );
  }
  graph.forEachNode((nodeId) => {
    if (nodeId === result.target?.uid) return;
    graph.setNodeAttribute(nodeId, "size", nodeSize(graph.degree(nodeId), 0.001));
  });

  return graph;
}
