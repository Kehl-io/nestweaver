import { useEffect } from "react";
import { useSigma } from "@react-sigma/core";
import { useStore } from "../../stores";
import { desaturate } from "./utils/graphColors";

export function GraphReducers() {
  const sigma = useSigma();
  const selectedNodeId = useStore((s) => s.selectedNodeId);
  const hoveredNodeId = useStore((s) => s.hoveredNodeId);
  const flowTraceActive = useStore((s) => s.flowTraceActive);
  const flowTraceNodeUids = useStore((s) => s.flowTraceNodeUids);
  const pathfindingActive = useStore((s) => s.pathfindingActive);
  const pathResults = useStore((s) => s.pathResults);
  const selectedPathIndex = useStore((s) => s.selectedPathIndex);

  useEffect(() => {
    const graph = sigma.getGraph();

    // Pre-compute path node set
    let pathNodeSet: Set<string> | null = null;
    if (
      pathfindingActive &&
      pathResults.length > 0 &&
      pathResults[selectedPathIndex]
    ) {
      pathNodeSet = new Set(pathResults[selectedPathIndex].nodes);
    }

    sigma.setSetting(
      "nodeReducer",
      (node: string, data: Record<string, any>) => {
        const res = { ...data };

        // Priority 1: Flow trace — dim non-trace nodes
        if (flowTraceActive && flowTraceNodeUids.length > 0) {
          if (!flowTraceNodeUids.includes(node)) {
            res.color = desaturate(data.color || "#999", 0.9);
            res.label = "";
            return res;
          }
          res.forceLabel = true;
        }

        // Priority 2: Pathfinding — dim non-path nodes
        if (pathNodeSet) {
          if (!pathNodeSet.has(node)) {
            res.color = desaturate(data.color || "#999", 0.9);
            res.label = "";
            return res;
          }
          res.forceLabel = true;
          res.highlighted = true;
        }

        // Priority 3: Hover — dim non-neighbors (only when no overlay is active)
        if (hoveredNodeId && !flowTraceActive && !pathNodeSet) {
          if (node === hoveredNodeId) {
            res.highlighted = true;
            res.forceLabel = true;
          } else if (
            graph.hasNode(hoveredNodeId) &&
            graph.areNeighbors(node, hoveredNodeId)
          ) {
            res.forceLabel = true;
          } else {
            res.color = desaturate(data.color || "#999", 0.8);
            res.label = "";
          }
        }

        // Always: selection highlight
        if (selectedNodeId === node) {
          res.highlighted = true;
          res.forceLabel = true;
        }

        return res;
      },
    );

    sigma.setSetting(
      "edgeReducer",
      (edge: string, data: Record<string, any>) => {
        const res = { ...data };

        // Only apply hover edge filtering when no overlay is active
        if (hoveredNodeId && !flowTraceActive && !pathNodeSet) {
          const extremities = graph.extremities(edge);
          if (!extremities.includes(hoveredNodeId)) {
            res.hidden = true;
          }
        }

        return res;
      },
    );

    sigma.refresh({ skipIndexation: true });
  }, [
    sigma,
    selectedNodeId,
    hoveredNodeId,
    flowTraceActive,
    flowTraceNodeUids,
    pathfindingActive,
    pathResults,
    selectedPathIndex,
  ]);

  return null;
}
