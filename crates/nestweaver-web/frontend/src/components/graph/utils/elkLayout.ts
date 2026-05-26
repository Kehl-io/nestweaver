import ELK from "elkjs/lib/elk.bundled.js";
import type Graph from "graphology";
import type { ElkNode } from "elkjs/lib/elk-api";

const elk = new ELK();

export async function applyElkLayout(
  graph: Graph,
  direction: "DOWN" | "RIGHT" = "DOWN",
): Promise<void> {
  const children = graph.mapNodes(
    (id: string, attrs: Record<string, unknown>) => ({
      id,
      width: ((attrs.size as number) || 10) * 3,
      height: ((attrs.size as number) || 10) * 2,
    }),
  );

  const edges = graph.mapEdges(
    (
      id: string,
      _attrs: Record<string, unknown>,
      source: string,
      target: string,
    ) => ({
      id,
      sources: [source],
      targets: [target],
    }),
  );

  const elkGraph: ElkNode = {
    id: "root",
    layoutOptions: {
      "elk.algorithm": "layered",
      "elk.direction": direction,
      "elk.spacing.nodeNode": "40",
      "elk.layered.spacing.nodeNodeBetweenLayers": "60",
    },
    children,
    edges,
  };

  const result = await elk.layout(elkGraph);

  for (const node of result.children || []) {
    if (graph.hasNode(node.id)) {
      graph.setNodeAttribute(node.id, "x", node.x || 0);
      graph.setNodeAttribute(node.id, "y", node.y || 0);
    }
  }
}
