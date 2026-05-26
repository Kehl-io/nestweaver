import Graph from "graphology";
import type { BrainContextResult } from "../../../api/types";
import {
  kindToColor,
  nodeSize,
  desaturate,
  relevanceToSaturation,
} from "./graphColors";

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

    graph.addNode(node.uid, {
      label: node.title || node.uid.split(":").pop() || node.uid,
      x: Math.random() * 100,
      y: Math.random() * 100,
      size: 6, // placeholder; finalized by finalizeNodeSizes after edges are added
      color,
      kind: node.kind,
      location: node.location,
      relevance: node.relevance,
      isSeed,
      forceLabel: isSeed,
    });
  }

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
