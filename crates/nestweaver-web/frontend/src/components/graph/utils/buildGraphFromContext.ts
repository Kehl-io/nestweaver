import Graph from "graphology";
import type { BrainContextResult } from "../../../api/types";
import {
  kindToColor,
  pprToSize,
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
      size: pprToSize(node.relevance),
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
