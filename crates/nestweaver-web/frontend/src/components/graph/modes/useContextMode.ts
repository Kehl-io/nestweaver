import { useCallback, useEffect, useRef } from "react";
import { useStore } from "../../../stores";
import { useForceLayout } from "../../../hooks/useForceLayout";
import { api } from "../../../api/client";
import { buildGraphFromContext, finalizeNodeSizes } from "../utils/buildGraphFromContext";

const MAX_LAYOUT_MS = 10_000;

export function useContextMode() {
  const setGraphData = useStore((s) => s.setGraphData);
  const seeds = useStore((s) => s.seeds);
  const graphMode = useStore((s) => s.graphMode);
  const stopTimerRef = useRef<ReturnType<typeof setTimeout> | null>(null);

  const { start, stop, kill, isRunning } = useForceLayout();

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
                type: "arrow",
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
                type: "arrow",
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

      finalizeNodeSizes(graph);
      setGraphData(graph);
      start(graph);
      // Stop after MAX_LAYOUT_MS as a safety ceiling
      if (stopTimerRef.current) clearTimeout(stopTimerRef.current);
      stopTimerRef.current = setTimeout(() => stop(), MAX_LAYOUT_MS);
    } catch (err) {
      console.error("Failed to load context:", err);
    }
  }, [graphMode, seeds, setGraphData, start, stop]);

  // Stop the layout automatically when isRunning goes false (convergence detected)
  useEffect(() => {
    if (!isRunning) {
      if (stopTimerRef.current) {
        clearTimeout(stopTimerRef.current);
        stopTimerRef.current = null;
      }
    }
  }, [isRunning]);

  useEffect(() => {
    loadContextData();
    return () => {
      if (stopTimerRef.current) clearTimeout(stopTimerRef.current);
      kill();
    };
  }, [loadContextData, kill]);

  return { isRunning, start, stop };
}
