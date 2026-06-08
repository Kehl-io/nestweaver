import { useCallback, useEffect, useState } from "react";
import { api } from "../../../api/client";
import type { OverviewResponse } from "../../../api/types";
import { useStore } from "../../../stores";
import { buildGraphFromOverview } from "../utils/buildGraphFromOverview";

export function useOverviewMode() {
  const graphMode = useStore((s) => s.graphMode);
  const setGraphData = useStore((s) => s.setGraphData);
  const [overview, setOverview] = useState<OverviewResponse | null>(null);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const loadOverview = useCallback(async () => {
    if (graphMode !== "overview") return;
    setLoading(true);
    setError(null);
    try {
      const result = await api.overview(24);
      setOverview(result);
      setGraphData(buildGraphFromOverview(result));
    } catch (err) {
      setError(err instanceof Error ? err.message : "Failed to load overview");
    } finally {
      setLoading(false);
    }
  }, [graphMode, setGraphData]);

  useEffect(() => {
    loadOverview();
  }, [loadOverview]);

  return { overview, loading, error, reload: loadOverview };
}
