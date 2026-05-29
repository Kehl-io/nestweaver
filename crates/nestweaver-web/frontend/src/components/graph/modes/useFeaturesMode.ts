import { useCallback, useEffect } from "react";
import { useStore } from "../../../stores";
import { api } from "../../../api/client";
import { buildGraphFromContext } from "../utils/buildGraphFromContext";

export function useFeaturesMode() {
  const setGraphData = useStore((s) => s.setGraphData);
  const graphMode = useStore((s) => s.graphMode);
  const seeds = useStore((s) => s.seeds);

  const loadFeaturesData = useCallback(async () => {
    if (graphMode !== "features" || seeds.length === 0) return;

    try {
      const result = await api.brainContext(seeds, 4000, "all");
      const graph = buildGraphFromContext(result);
      setGraphData(graph);
    } catch (err) {
      console.error("Failed to load features:", err);
    }
  }, [graphMode, seeds, setGraphData]);

  useEffect(() => {
    loadFeaturesData();
  }, [loadFeaturesData]);
}
