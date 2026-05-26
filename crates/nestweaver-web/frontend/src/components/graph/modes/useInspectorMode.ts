import { useCallback, useEffect, useState } from "react";
import { useLoadGraph } from "@react-sigma/core";
import { useStore } from "../../../stores";
import { api } from "../../../api/client";
import { buildGraphFromContext } from "../utils/buildGraphFromContext";
import type { BrainContextResult } from "../../../api/types";

export function useInspectorMode() {
  const loadGraph = useLoadGraph();
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
      loadGraph(graph);
    } catch (err) {
      console.error("Failed to load inspector:", err);
    }
  }, [graphMode, seeds, tokenBudget, loadGraph]);

  useEffect(() => {
    loadInspectorData();
  }, [loadInspectorData]);

  return { contextResult, tokenBudget, setTokenBudget };
}
