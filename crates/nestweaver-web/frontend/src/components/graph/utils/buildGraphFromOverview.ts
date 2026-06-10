import Graph from "graphology";
import type { OverviewResponse, OverviewLandmark } from "../../../api/types";
import { kindToColor, nodeSize } from "./graphColors";

function landmarkColor(item: OverviewLandmark): string {
  if (item.kind === "repo") return kindToColor("Section");
  if (item.kind === "service") return kindToColor("Interface");
  if (item.kind === "symbol") return kindToColor("Function");
  if (item.kind === "note") return kindToColor("Note");
  return kindToColor(item.kind);
}

export function buildGraphFromOverview(result: OverviewResponse): Graph {
  const graph = new Graph({ type: "directed", multi: true });
  const maxScore = Math.max(...result.landmarks.map((n) => n.score), 0.001);
  const hubItems = result.landmarks.filter(
    (item) => item.kind === "repo" || item.kind === "service",
  );
  const orbitItems = result.landmarks.filter(
    (item) => item.kind !== "repo" && item.kind !== "service",
  );

  for (let i = 0; i < result.landmarks.length; i++) {
    const item = result.landmarks[i];
    const isHub = item.kind === "repo" || item.kind === "service";
    const group = isHub ? hubItems : orbitItems;
    const groupIndex = group.findIndex((candidate) => candidate.uid === item.uid);
    const angle =
      (groupIndex / Math.max(group.length, 1)) * Math.PI * 2;
    const ring = isHub
      ? hubItems.length <= 1 ? 0 : 64
      : 96 + (groupIndex % 3) * 14;
    const overviewOffsetX = 62;
    const overviewOffsetY = 12;
    const normalized = Math.max(item.score / maxScore, 0.08);

    graph.addNode(item.uid, {
      label: item.label,
      x: overviewOffsetX + Math.cos(angle) * ring,
      y: overviewOffsetY + Math.sin(angle) * ring,
      size: nodeSize(isHub ? 3 : 1, normalized) * (isHub ? 1.18 : 1.08),
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

  const primaryHub = hubItems[0];
  if (primaryHub && graph.hasNode(primaryHub.uid)) {
    for (const item of orbitItems) {
      if (graph.hasNode(item.uid)) {
        graph.addEdge(primaryHub.uid, item.uid, {
          type: "overview",
          confidence: Math.max(item.score / maxScore, 0.18),
        });
      }
    }
  }

  return graph;
}
