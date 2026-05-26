import { useCallback, useEffect, useRef, useState } from "react";
import { useSigma, useLoadGraph } from "@react-sigma/core";
import { useWorkerLayoutForceAtlas2 } from "@react-sigma/layout-forceatlas2";
import { useStore } from "../../../stores";
import { api } from "../../../api/client";
import { buildGraphFromContext, finalizeNodeSizes } from "../utils/buildGraphFromContext";

const HOP_BUDGETS: Record<number, number> = { 1: 500, 2: 2000, 3: 4000, 4: 8000 };
const MAX_LAYOUT_MS = 10_000;

export function useLocalMode() {
  const sigma = useSigma();
  const loadGraph = useLoadGraph();
  const graphMode = useStore((s) => s.graphMode);
  const selectedNodeId = useStore((s) => s.selectedNodeId);
  const forceParams = useStore((s) => s.forceParams);
  const [hops, setHops] = useState(2);
  const stopTimerRef = useRef<ReturnType<typeof setTimeout> | null>(null);

  const { start, stop, kill, isRunning } = useWorkerLayoutForceAtlas2({
    settings: {
      slowDown: forceParams.settling,
      gravity: forceParams.gravity,
      scalingRatio: forceParams.repulsion,
      barnesHutOptimize: true,
    },
  });

  const loadLocalData = useCallback(async () => {
    if (graphMode !== "local" || !selectedNodeId) return;

    const budget = HOP_BUDGETS[hops] ?? 2000;

    try {
      const result = await api.brainContext([selectedNodeId], budget, "all");
      const graph = buildGraphFromContext(result);
      finalizeNodeSizes(graph);

      // Pin seed node at center
      if (graph.hasNode(selectedNodeId)) {
        graph.setNodeAttribute(selectedNodeId, "x", 0);
        graph.setNodeAttribute(selectedNodeId, "y", 0);
      }

      loadGraph(graph);
      start();
      // Stop after MAX_LAYOUT_MS as a safety ceiling
      if (stopTimerRef.current) clearTimeout(stopTimerRef.current);
      stopTimerRef.current = setTimeout(() => stop(), MAX_LAYOUT_MS);
      // Camera fit after layout settles
      setTimeout(() => sigma.getCamera().animatedReset(), 100);
    } catch (err) {
      console.error("Failed to load local graph:", err);
    }
  }, [graphMode, selectedNodeId, hops, loadGraph, start, stop, sigma]);

  // Clear stop timer when layout converges
  useEffect(() => {
    if (!isRunning) {
      if (stopTimerRef.current) {
        clearTimeout(stopTimerRef.current);
        stopTimerRef.current = null;
      }
    }
  }, [isRunning]);

  useEffect(() => {
    loadLocalData();
    return () => {
      if (stopTimerRef.current) clearTimeout(stopTimerRef.current);
      kill();
    };
  }, [loadLocalData, kill]);

  return { hops, setHops, isRunning, start, stop };
}
