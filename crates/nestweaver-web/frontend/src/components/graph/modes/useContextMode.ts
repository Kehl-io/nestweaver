import { useCallback, useEffect, useRef } from "react";
import { useStore } from "../../../stores";
import { useForceLayout } from "../../../hooks/useForceLayout";
import { api } from "../../../api/client";
import { buildGraphFromContext, finalizeNodeSizes } from "../utils/buildGraphFromContext";
import { preserveGraphLayout } from "../utils/preserveGraphLayout";

const MAX_LAYOUT_MS = 10_000;

function sameSeeds(left: string[], right: string[]): boolean {
  if (left.length !== right.length) return false;
  return left.every((seed, index) => seed === right[index]);
}

function loadErrorMessage(err: unknown, fallback: string): string {
  return err instanceof Error && err.message ? err.message : fallback;
}

export function useContextMode() {
  const setGraphData = useStore((s) => s.setGraphData);
  const notify = useStore((s) => s.notify);
  const seeds = useStore((s) => s.seeds);
  const graphMode = useStore((s) => s.graphMode);
  const stopTimerRef = useRef<ReturnType<typeof setTimeout> | null>(null);
  const requestIdRef = useRef(0);

  const { start, stop, kill, isRunning } = useForceLayout();

  const loadContextData = useCallback(async () => {
    if (graphMode !== "context" || seeds.length === 0) {
      requestIdRef.current += 1;
      return;
    }

    const requestId = ++requestIdRef.current;
    const requestSeeds = [...seeds];
    const isCurrentRequest = () => {
      const state = useStore.getState();
      return (
        requestId === requestIdRef.current &&
        state.graphMode === "context" &&
        sameSeeds(state.seeds, requestSeeds)
      );
    };

    try {
      const result = await api.brainContext(requestSeeds, 2000, "all");
      if (!isCurrentRequest()) return;

      const graph = buildGraphFromContext(result);

      for (const seed of result.seeds) {
        try {
          const detail = await api.symbol(seed.uid);
          if (!isCurrentRequest()) return;

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
      preserveGraphLayout(graph, useStore.getState().graphInstance);
      if (!isCurrentRequest()) return;

      setGraphData(graph);
      start(graph);
      // Stop after MAX_LAYOUT_MS as a safety ceiling
      if (stopTimerRef.current) clearTimeout(stopTimerRef.current);
      stopTimerRef.current = setTimeout(() => stop(), MAX_LAYOUT_MS);
    } catch (err) {
      if (!isCurrentRequest()) return;

      console.error("Failed to load context:", err);
      notify({
        kind: "error",
        title: "Context graph failed",
        message: loadErrorMessage(err, "Failed to load context graph"),
      });
    }
  }, [graphMode, notify, seeds, setGraphData, start, stop]);

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
