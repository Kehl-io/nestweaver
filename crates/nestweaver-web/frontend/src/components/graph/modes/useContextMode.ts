import { useCallback, useEffect } from "react";
import { useLoadGraph } from "@react-sigma/core";
import { useWorkerLayoutForceAtlas2 } from "@react-sigma/layout-forceatlas2";
import { useStore } from "../../../stores";
import { api } from "../../../api/client";
import { buildGraphFromContext } from "../utils/buildGraphFromContext";

export function useContextMode() {
  const loadGraph = useLoadGraph();
  const seeds = useStore((s) => s.seeds);
  const graphMode = useStore((s) => s.graphMode);

  const { start, stop, kill, isRunning } = useWorkerLayoutForceAtlas2({
    settings: {
      slowDown: 10,
      gravity: 1,
      scalingRatio: 2,
      barnesHutOptimize: true,
    },
  });

  const loadContextData = useCallback(async () => {
    if (graphMode !== "context" || seeds.length === 0) return;

    try {
      const result = await api.brainContext(seeds, 2000, "all");
      const graph = buildGraphFromContext(result);

      for (const seed of result.seeds) {
        try {
          const detail = await api.symbol(seed.uid);
          for (const caller of detail.callers) {
            if (
              graph.hasNode(caller.uid) &&
              !graph.hasEdge(caller.uid, seed.uid)
            ) {
              graph.addEdge(caller.uid, seed.uid, {
                size: 1.5,
                color: "#9CA3AF",
                label: "calls",
              });
            }
          }
          for (const callee of detail.callees) {
            if (
              graph.hasNode(callee.uid) &&
              !graph.hasEdge(seed.uid, callee.uid)
            ) {
              graph.addEdge(seed.uid, callee.uid, {
                size: 1.5,
                color: "#9CA3AF",
                label: "calls",
              });
            }
          }
        } catch {
          // Symbol lookup fails for notes/tags — skip
        }
      }

      loadGraph(graph);
      start();
      setTimeout(() => stop(), 3000);
    } catch (err) {
      console.error("Failed to load context:", err);
    }
  }, [graphMode, seeds, loadGraph, start, stop]);

  useEffect(() => {
    loadContextData();
    return () => {
      kill();
    };
  }, [loadContextData, kill]);

  return { isRunning, start, stop };
}
