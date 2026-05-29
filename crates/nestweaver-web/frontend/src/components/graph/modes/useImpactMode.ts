import { useCallback, useEffect } from "react";
import { useStore } from "../../../stores";
import { api } from "../../../api/client";
import { buildGraphFromImpact } from "../utils/buildGraphFromImpact";
import { applyElkLayout } from "../utils/elkLayout";

export function useImpactMode() {
  const setGraphData = useStore((s) => s.setGraphData);
  const selectedNodeId = useStore((s) => s.selectedNodeId);
  const graphMode = useStore((s) => s.graphMode);

  const loadImpactData = useCallback(async () => {
    if (graphMode !== "impact" || !selectedNodeId) return;

    try {
      const nodes = await api.impact(selectedNodeId, 3, 0.3);
      let targetName = selectedNodeId;
      try {
        const detail = await api.symbol(selectedNodeId);
        targetName = detail.symbol.name;
      } catch {
        /* selected node may not be a symbol */
      }

      const graph = buildGraphFromImpact(selectedNodeId, targetName, nodes);
      await applyElkLayout(graph, "DOWN");
      setGraphData(graph);
    } catch (err) {
      console.error("Failed to load impact:", err);
    }
  }, [graphMode, selectedNodeId, setGraphData]);

  useEffect(() => {
    loadImpactData();
  }, [loadImpactData]);
}
