import { useCallback, useEffect, useRef, useState } from "react";
import type Graph from "graphology";
import type { OverviewResponse } from "../../../api/types";
import type { SceneMetadata } from "../../../api/p1Types";
import { appendWorkspaceParam } from "../../../api/workspaces";
import { useStore } from "../../../stores";
import { buildGraphFromOverview } from "../utils/buildGraphFromOverview";
import { preserveGraphLayout } from "../utils/preserveGraphLayout";

type ScopedOverviewResponse = OverviewResponse & { _meta?: SceneMetadata };

function loadErrorMessage(err: unknown, fallback: string): string {
  return err instanceof Error && err.message ? err.message : fallback;
}

async function loadScopedOverview(
  limit: number,
  workspaceId: string,
): Promise<ScopedOverviewResponse> {
  const url = appendWorkspaceParam(`/api/v1/overview?limit=${limit}`, workspaceId);
  const response = await fetch(url);
  if (!response.ok) {
    const body = await response.json().catch(() => ({ error: response.statusText }));
    throw new Error(body.error || response.statusText);
  }
  return response.json() as Promise<ScopedOverviewResponse>;
}

export function useOverviewMode() {
  const graphMode = useStore((s) => s.graphMode);
  const setGraphData = useStore((s) => s.setGraphData);
  const activeWorkspaceId = useStore((s) => s.activeWorkspaceId);
  const setActiveLens = useStore((s) => s.setActiveLens);
  const setSceneMetadata = useStore((s) => s.setSceneMetadata);
  const notify = useStore((s) => s.notify);
  const [overview, setOverview] = useState<ScopedOverviewResponse | null>(null);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const requestIdRef = useRef(0);
  const previousOverviewGraphRef = useRef<{ workspaceId: string; graph: Graph } | null>(null);

  const loadOverview = useCallback(async () => {
    if (graphMode !== "overview") {
      requestIdRef.current += 1;
      setLoading(false);
      return;
    }

    const requestId = ++requestIdRef.current;
    const requestWorkspaceId = activeWorkspaceId || "all";
    const isCurrentRequest = () =>
      requestId === requestIdRef.current &&
      useStore.getState().graphMode === "overview" &&
      useStore.getState().activeWorkspaceId === requestWorkspaceId;

    setLoading(true);
    setError(null);
    try {
      const result = await loadScopedOverview(24, requestWorkspaceId);
      if (!isCurrentRequest()) return;

      const graph = buildGraphFromOverview(result);
      const previous = previousOverviewGraphRef.current;
      preserveGraphLayout(
        graph,
        previous?.workspaceId === requestWorkspaceId ? previous.graph : null,
        {
          keepExistingNewNodePositions: true,
        },
      );

      setOverview(result);
      setActiveLens({
        lens: "overview",
        label: "Overview",
        targetUid: null,
        workspaceId: requestWorkspaceId,
      });
      setSceneMetadata(result._meta ?? null);
      setGraphData(graph);
      previousOverviewGraphRef.current = { workspaceId: requestWorkspaceId, graph };
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
  }, [activeWorkspaceId, graphMode, notify, setActiveLens, setGraphData, setSceneMetadata]);

  useEffect(() => {
    loadOverview();
    return () => {
      requestIdRef.current += 1;
    };
  }, [loadOverview]);

  return { overview, loading, error, reload: loadOverview };
}
