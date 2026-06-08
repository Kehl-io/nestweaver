import Graph from "graphology";
import type { OverviewResponse, OverviewLandmark } from "../../../api/types";
import { kindToColor, nodeSize } from "./graphColors";

function landmarkColor(item: OverviewLandmark): string {
  if (item.kind === "repo") return "#6B7280";
  if (item.kind === "service") return "#3B82F6";
  if (item.kind === "symbol") return "#3B82F6";
  if (item.kind === "note") return "#78716C";
  return kindToColor(item.kind);
}

export function buildGraphFromOverview(result: OverviewResponse): Graph {
  const graph = new Graph({ type: "directed", multi: true });
  const maxScore = Math.max(...result.landmarks.map((n) => n.score), 0.001);

  for (let i = 0; i < result.landmarks.length; i++) {
    const item = result.landmarks[i];
    const angle = (i / Math.max(result.landmarks.length, 1)) * Math.PI * 2;
    const ring = item.kind === "repo" || item.kind === "service" ? 220 : 120;
    const normalized = Math.max(item.score / maxScore, 0.08);

    graph.addNode(item.uid, {
      label: item.label,
      x: Math.cos(angle) * ring,
      y: Math.sin(angle) * ring,
      size: nodeSize(1, normalized),
      color: landmarkColor(item),
      kind: item.kind,
      location: item.location,
      relevance: item.score,
      reason: item.reason,
      forceLabel: i < 8,
      // Current label filtering preserves seed labels through zoom changes.
      isSeed: i < 8,
      isOverview: true,
    });
  }

  return graph;
}
