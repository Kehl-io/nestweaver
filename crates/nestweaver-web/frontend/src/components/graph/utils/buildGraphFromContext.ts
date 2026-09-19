import Graph from "graphology";
import type { BrainContextResult } from "../../../api/types";
import {
  kindToColor,
  nodeSize,
  desaturate,
  relevanceToSaturation,
  EDGE_COLORS,
} from "./graphColors";
import { deterministicGraphPosition } from "./preserveGraphLayout";

export function buildGraphFromContext(result: BrainContextResult): Graph {
  const graph = new Graph({ type: "directed", multi: true });
  const allNodes = [...result.seeds, ...result.connected];
  const maxRelevance = Math.max(...allNodes.map((n) => n.relevance), 0.001);

  for (const node of allNodes) {
    if (graph.hasNode(node.uid)) continue;

    const isSeed = result.seeds.some((s) => s.uid === node.uid);
    const baseColor = kindToColor(node.kind);
    const satAmount = isSeed
      ? 0
      : relevanceToSaturation(node.relevance, maxRelevance);
    const color = desaturate(baseColor, satAmount);
    const position = deterministicGraphPosition(node.uid);

    graph.addNode(node.uid, {
      label: node.title || node.uid.split(":").pop() || node.uid,
      x: position.x,
      y: position.y,
      size: 6, // placeholder; finalized by finalizeNodeSizes after edges are added
      color,
      // paletteKind lets the graph bridge re-derive theme-correct colors when
      // the theme flips mid-scene; colorDesaturate preserves the relevance fade
      paletteKind: node.kind,
      colorDesaturate: satAmount,
      kind: node.kind,
      location: node.location,
      relevance: node.relevance,
      bridgeScore: node.bridge_score ?? 0,
      isSeed,
      forceLabel: isSeed,
    });
  }

  for (const [index, edge] of (result.edges ?? []).entries()) {
    if (!graph.hasNode(edge.source) || !graph.hasNode(edge.target)) {
      throw new Error("Context relationship refers to an unavailable node.");
    }
    graph.addDirectedEdgeWithKey(`context:${index}`, edge.source, edge.target, {
      edgeType: edge.edge_type,
      label: edge.edge_type,
      confidence: edge.confidence,
      evidence: edge.evidence,
      color: EDGE_COLORS[edge.edge_type.toLowerCase()] ?? "#6b7280",
      size: 1,
    });
  }
  graph.setAttribute("contextGraphMeta", result.graph_meta ?? null);
  return graph;
}

/** Update every node's size based on its degree and relevance (pagerank proxy).
 *  Call this after all edges have been added to the graph. */
export function finalizeNodeSizes(graph: Graph): void {
  graph.forEachNode((nodeId) => {
    const relevance = (graph.getNodeAttribute(nodeId, "relevance") as number | undefined) ?? 0;
    graph.setNodeAttribute(nodeId, "size", nodeSize(graph.degree(nodeId), relevance));
  });
}
