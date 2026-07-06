import { useCallback, useEffect, useRef, useState } from "react";
import { api } from "../../../api/client";
import type { OverviewResponse } from "../../../api/types";
import { useStore } from "../../../stores";
import { buildGraphFromOverview } from "../utils/buildGraphFromOverview";
import { preserveGraphLayout } from "../utils/preserveGraphLayout";

function loadErrorMessage(err: unknown, fallback: string): string {
  return err instanceof Error && err.message ? err.message : fallback;
}

export function useOverviewMode() {
  const graphMode = useStore((s) => s.graphMode);
  const setGraphData = useStore((s) => s.setGraphData);
  const notify = useStore((s) => s.notify);
  const [overview, setOverview] = useState<OverviewResponse | null>(null);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const requestIdRef = useRef(0);

  const loadOverview = useCallback(async () => {
    if (graphMode !== "overview") {
      requestIdRef.current += 1;
      setLoading(false);
      return;
    }

    const requestId = ++requestIdRef.current;
    const isCurrentRequest = () =>
      requestId === requestIdRef.current &&
      useStore.getState().graphMode === "overview";

    setLoading(true);
    setError(null);
    try {
      const result = await api.overview(24);
      if (!isCurrentRequest()) return;

      const graph = buildGraphFromOverview(result);
      preserveGraphLayout(graph, useStore.getState().graphInstance, {
        keepExistingNewNodePositions: true,
      });

      setOverview(result);
      setGraphData(graph);
    } catch (err) {
      if (!isCurrentRequest()) return;

      const message = loadErrorMessage(err, "Failed to load overview");
      setError(message);
      notify({
        kind: "error",
        title: "Overview failed",
        message,
      });
    } finally {
      if (requestId === requestIdRef.current) {
        setLoading(false);
      }
    }
  }, [graphMode, notify, setGraphData]);

  useEffect(() => {
    loadOverview();
  }, [loadOverview]);

  return { overview, loading, error, reload: loadOverview };
}
