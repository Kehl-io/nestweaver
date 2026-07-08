import { useCallback, useEffect, useRef, useState } from "react";
import type Graph from "graphology";
import type { OverviewResponse } from "../../../api/types";
import type { SceneMetadata } from "../../../api/p1Types";
import {
  appendWorkspaceParam,
  workspaceSceneMetadataWithResult,
} from "../../../api/workspaces";
import { useStore } from "../../../stores";
import { useForceLayout } from "../../../hooks/useForceLayout";
import { buildGraphFromOverview } from "../utils/buildGraphFromOverview";
import { preserveGraphLayout } from "../utils/preserveGraphLayout";

const MAX_LAYOUT_MS = 10_000;

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
  const clearGraphData = useStore((s) => s.clearGraphData);
  const activeWorkspaceId = useStore((s) => s.activeWorkspaceId);
  const setActiveLens = useStore((s) => s.setActiveLens);
  const setSceneMetadata = useStore((s) => s.setSceneMetadata);
  const notify = useStore((s) => s.notify);
  const [overview, setOverview] = useState<ScopedOverviewResponse | null>(null);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const requestIdRef = useRef(0);
  const previousOverviewGraphRef = useRef<{ workspaceId: string; graph: Graph } | null>(null);
  const stopTimerRef = useRef<ReturnType<typeof setTimeout> | null>(null);
  const { start, stop, kill } = useForceLayout();

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
    const currentState = useStore.getState();
    const previousWorkspaceId =
      currentState.sceneMetadata?.workspace_id ??
      currentState.activeLens.workspaceId ??
      "all";
    if (previousWorkspaceId !== requestWorkspaceId) {
      setOverview(null);
      clearGraphData();
      setSceneMetadata(
        workspaceSceneMetadataWithResult(
          currentState.selectedWorkspace()?._meta,
          "loading",
          `Loading ${currentState.selectedWorkspace()?.label ?? requestWorkspaceId}.`,
        ),
      );
    }
    try {
      // Starfield density: the constellation should feel populated. Server
      // clamps at 100; per-galaxy caps in the builder keep it readable.
      const result = await loadScopedOverview(96, requestWorkspaceId);
      if (!isCurrentRequest()) return;

      const graph = buildGraphFromOverview(result);
      const previous = previousOverviewGraphRef.current;
      const hasPreviousLayout = previous?.workspaceId === requestWorkspaceId;
      preserveGraphLayout(
        graph,
        hasPreviousLayout ? previous.graph : null,
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
      // Settle fresh constellations organically; preserved layouts stay frozen
      // (object constancy), and reduced-effects users keep the static seed layout.
      if (!hasPreviousLayout && !useStore.getState().reducedEffects) {
        start(graph);
        if (stopTimerRef.current) clearTimeout(stopTimerRef.current);
        stopTimerRef.current = setTimeout(() => stop(), MAX_LAYOUT_MS);
      }
    } catch (err) {
      if (!isCurrentRequest()) return;

      const message = loadErrorMessage(err, "Failed to load overview");
      setError(message);
      setOverview(null);
      clearGraphData();
      setSceneMetadata(
        workspaceSceneMetadataWithResult(
          useStore.getState().selectedWorkspace()?._meta,
          "error",
          message,
        ),
      );
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
  }, [
    activeWorkspaceId,
    clearGraphData,
    graphMode,
    notify,
    setActiveLens,
    setGraphData,
    setSceneMetadata,
    start,
    stop,
  ]);

  useEffect(() => {
    loadOverview();
    return () => {
      requestIdRef.current += 1;
      if (stopTimerRef.current) clearTimeout(stopTimerRef.current);
      kill();
    };
  }, [loadOverview, kill]);

  return { overview, loading, error, reload: loadOverview };
}
