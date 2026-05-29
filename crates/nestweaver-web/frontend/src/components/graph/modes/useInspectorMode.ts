import { useCallback, useEffect, useState } from "react";
import { useStore } from "../../../stores";
import { api } from "../../../api/client";
import { buildGraphFromContext, finalizeNodeSizes } from "../utils/buildGraphFromContext";
import type { BrainContextResult } from "../../../api/types";

export function useInspectorMode() {
  const setGraphData = useStore((s) => s.setGraphData);
  const graphMode = useStore((s) => s.graphMode);
  const seeds = useStore((s) => s.seeds);
  const [contextResult, setContextResult] =
    useState<BrainContextResult | null>(null);
  const [tokenBudget, setTokenBudget] = useState(2000);

  const loadInspectorData = useCallback(async () => {
    if (graphMode !== "inspector" || seeds.length === 0) return;

    try {
      const result = await api.brainContext(seeds, tokenBudget, "all");
      setContextResult(result);
      const graph = buildGraphFromContext(result);
      finalizeNodeSizes(graph);
      setGraphData(graph);
    } catch (err) {
      console.error("Failed to load inspector:", err);
    }
  }, [graphMode, seeds, tokenBudget, setGraphData]);

  useEffect(() => {
    loadInspectorData();
  }, [loadInspectorData]);

  return { contextResult, tokenBudget, setTokenBudget };
}
