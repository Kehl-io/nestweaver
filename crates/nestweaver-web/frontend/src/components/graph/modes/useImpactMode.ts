import { useCallback, useEffect, useRef } from "react";
import type Graph from "graphology";
import { useStore } from "../../../stores";
import { api } from "../../../api/client";
import { buildGraphFromImpact } from "../utils/buildGraphFromImpact";
import { applyElkLayout } from "../utils/elkLayout";
import { preserveGraphLayout } from "../utils/preserveGraphLayout";

function loadErrorMessage(err: unknown, fallback: string): string {
  return err instanceof Error && err.message ? err.message : fallback;
}

export function useImpactMode() {
  const setGraphData = useStore((s) => s.setGraphData);
  const notify = useStore((s) => s.notify);
  const selectedNodeId = useStore((s) => s.selectedNodeId);
  const graphMode = useStore((s) => s.graphMode);
  const requestIdRef = useRef(0);
  const previousLayoutRef = useRef<{ targetNodeId: string; graph: Graph } | null>(
    null,
  );

  const loadImpactData = useCallback(async () => {
    if (graphMode !== "impact" || !selectedNodeId) {
      requestIdRef.current += 1;
      return;
    }

    const requestId = ++requestIdRef.current;
    const targetNodeId = selectedNodeId;
    const isCurrentRequest = () => {
      const state = useStore.getState();
      return (
        requestId === requestIdRef.current &&
        state.graphMode === "impact" &&
        state.selectedNodeId === targetNodeId
      );
    };

    try {
      const nodes = await api.impact(targetNodeId, 3, 0.3);
      if (!isCurrentRequest()) return;

      let targetName = targetNodeId;
      try {
        const detail = await api.symbol(targetNodeId);
        if (!isCurrentRequest()) return;
        targetName = detail.symbol.name;
      } catch {
        if (!isCurrentRequest()) return;
        /* selected node may not be a symbol */
      }

      const graph = buildGraphFromImpact(targetNodeId, targetName, nodes);
      await applyElkLayout(graph, "DOWN");
      if (!isCurrentRequest()) return;

      const previousLayout = previousLayoutRef.current;
      preserveGraphLayout(
        graph,
        previousLayout?.targetNodeId === targetNodeId
          ? previousLayout.graph
          : null,
        {
          keepExistingNewNodePositions: true,
        },
      );
      if (!isCurrentRequest()) return;

      setGraphData(graph);
      previousLayoutRef.current = { targetNodeId, graph };
    } catch (err) {
      if (!isCurrentRequest()) return;

      console.error("Failed to load impact:", err);
      notify({
        kind: "error",
        title: "Impact graph failed",
        message: loadErrorMessage(err, "Failed to load impact graph"),
      });
    }
  }, [graphMode, notify, selectedNodeId, setGraphData]);

  useEffect(() => {
    loadImpactData();
  }, [loadImpactData]);
}
