import { useCallback, useEffect, useRef, useState } from "react";
import { useStore } from "../../../stores";
import { useForceLayout } from "../../../hooks/useForceLayout";
import { api } from "../../../api/client";
import { buildGraphFromContext, finalizeNodeSizes } from "../utils/buildGraphFromContext";

const HOP_BUDGETS: Record<number, number> = { 1: 500, 2: 2000, 3: 4000, 4: 8000 };
const MAX_LAYOUT_MS = 10_000;

export function useLocalMode() {
  const setGraphData = useStore((s) => s.setGraphData);
  const graphMode = useStore((s) => s.graphMode);
  const selectedNodeId = useStore((s) => s.selectedNodeId);
  const [hops, setHops] = useState(2);
  const stopTimerRef = useRef<ReturnType<typeof setTimeout> | null>(null);

  const { start, stop, kill, isRunning } = useForceLayout();

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

      setGraphData(graph);
      start(graph);
      // Stop after MAX_LAYOUT_MS as a safety ceiling
      if (stopTimerRef.current) clearTimeout(stopTimerRef.current);
      stopTimerRef.current = setTimeout(() => stop(), MAX_LAYOUT_MS);
    } catch (err) {
      console.error("Failed to load local graph:", err);
    }
  }, [graphMode, selectedNodeId, hops, setGraphData, start, stop]);

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
