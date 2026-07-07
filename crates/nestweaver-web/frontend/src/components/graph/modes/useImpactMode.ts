import { useCallback, useEffect, useRef } from "react";
import type Graph from "graphology";
import { useStore } from "../../../stores";
import { loadImpactLens } from "../../../api/impactLens";
import { workspaceSceneMetadataWithResult } from "../../../api/workspaces";
import { buildGraphFromImpact } from "../utils/buildGraphFromImpact";
import { applyElkLayout } from "../utils/elkLayout";
import { preserveGraphLayout } from "../utils/preserveGraphLayout";

function loadErrorMessage(err: unknown, fallback: string): string {
  return err instanceof Error && err.message ? err.message : fallback;
}

export function useImpactMode() {
  const setGraphData = useStore((s) => s.setGraphData);
  const clearGraphData = useStore((s) => s.clearGraphData);
  const notify = useStore((s) => s.notify);
  const selectedNodeId = useStore((s) => s.selectedNodeId);
  const graphMode = useStore((s) => s.graphMode);
  const activeWorkspaceId = useStore((s) => s.activeWorkspaceId);
  const setActiveLens = useStore((s) => s.setActiveLens);
  const setSceneMetadata = useStore((s) => s.setSceneMetadata);
  const requestIdRef = useRef(0);
  const previousLayoutRef = useRef<{ key: string; graph: Graph } | null>(
    null,
  );

  const loadImpactData = useCallback(async () => {
    if (graphMode !== "impact" || !selectedNodeId) {
      requestIdRef.current += 1;
      return;
    }

    const requestId = ++requestIdRef.current;
    const targetNodeId = selectedNodeId;
    const requestWorkspaceId = activeWorkspaceId || "all";
    const layoutKey = `${requestWorkspaceId}:${targetNodeId}`;
    const isCurrentRequest = () => {
      const state = useStore.getState();
      return (
        requestId === requestIdRef.current &&
        state.graphMode === "impact" &&
        state.selectedNodeId === targetNodeId &&
        state.activeWorkspaceId === requestWorkspaceId
      );
    };

    try {
      const currentState = useStore.getState();
      const previousWorkspaceId =
        currentState.sceneMetadata?.workspace_id ??
        currentState.activeLens.workspaceId ??
        "all";
      const previousTargetId = currentState.activeLens.targetUid ?? null;
      if (
        previousWorkspaceId !== requestWorkspaceId ||
        previousTargetId !== targetNodeId
      ) {
        clearGraphData();
        setSceneMetadata(
          workspaceSceneMetadataWithResult(
            currentState.selectedWorkspace()?._meta,
            "loading",
            `Loading impact for ${targetNodeId}.`,
          ),
        );
      }

      const result = await loadImpactLens(targetNodeId, {
        depth: 3,
        confidence: 0.3,
        workspaceId: requestWorkspaceId,
      });
      if (!isCurrentRequest()) return;

      const graph = buildGraphFromImpact(result);
      await applyElkLayout(graph, "DOWN");
      if (!isCurrentRequest()) return;

      const previousLayout = previousLayoutRef.current;
      preserveGraphLayout(
        graph,
        previousLayout?.key === layoutKey ? previousLayout.graph : null,
        {
          keepExistingNewNodePositions: true,
        },
      );
      if (!isCurrentRequest()) return;

      setGraphData(graph);
      setActiveLens({
        lens: "impact",
        label: `Impact: ${result.target?.name ?? targetNodeId}`,
        targetUid: targetNodeId,
        workspaceId: requestWorkspaceId,
      });
      setSceneMetadata(result._meta ?? null);
      previousLayoutRef.current = { key: layoutKey, graph };
    } catch (err) {
      if (!isCurrentRequest()) return;

      console.error("Failed to load impact:", err);
      const message = loadErrorMessage(err, "Failed to load impact graph");
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
        title: "Impact graph failed",
        message,
      });
    }
  }, [
    activeWorkspaceId,
    clearGraphData,
    graphMode,
    notify,
    selectedNodeId,
    setActiveLens,
    setGraphData,
    setSceneMetadata,
  ]);

  useEffect(() => {
    loadImpactData();
    return () => {
      requestIdRef.current += 1;
    };
  }, [loadImpactData]);
}
