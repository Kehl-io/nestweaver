import { useCallback, useEffect, useRef } from "react";
import type Graph from "graphology";
import { useStore } from "../../../stores";
import { ImpactTimeoutError, loadImpactLens } from "../../../api/impactLens";
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
  const impactDepth = useStore((s) => s.impactDepth);
  const impactConfidence = useStore((s) => s.impactConfidence);
  const setActiveLens = useStore((s) => s.setActiveLens);
  const setSceneMetadata = useStore((s) => s.setSceneMetadata);
  // Re-run the current impact query once a debounced PageRank recompute lands,
  // so a timed-out/stale graph fills in when ranks are ready (nw-029).
  const ranksGeneration = useStore((s) => s.ranksGeneration);
  const requestIdRef = useRef(0);
  const abortRef = useRef<AbortController | null>(null);
  const previousLayoutRef = useRef<{ key: string; graph: Graph } | null>(
    null,
  );

  const loadImpactData = useCallback(async () => {
    // Any prior in-flight request is now superseded — abort its fetch so we
    // don't leave a hung PageRank request running against a cold DB.
    abortRef.current?.abort();

    if (graphMode !== "impact" || !selectedNodeId) {
      requestIdRef.current += 1;
      abortRef.current = null;
      return;
    }

    const controller = new AbortController();
    abortRef.current = controller;

    setActiveLens({ lens: "impact", label: "Impact", targetUid: selectedNodeId, workspaceId: activeWorkspaceId || "all" });

    const requestId = ++requestIdRef.current;
    const targetNodeId = selectedNodeId;
    const requestWorkspaceId = activeWorkspaceId || "all";
    const layoutKey = `${requestWorkspaceId}:${targetNodeId}`;
    const requestDepth = impactDepth;
    const requestConfidence = impactConfidence;
    const isCurrentRequest = () => {
      const state = useStore.getState();
      return (
        requestId === requestIdRef.current &&
        state.graphMode === "impact" &&
        state.selectedNodeId === targetNodeId &&
        state.activeWorkspaceId === requestWorkspaceId &&
        state.impactDepth === requestDepth &&
        state.impactConfidence === requestConfidence
      );
    };

    try {
      // Show the loading scene on EVERY new query. Previously this only fired
      // when the workspace/target changed, which silently kept the prior graph
      // on-screen while a re-query hung — an unhonest loading state.
      clearGraphData();
      setSceneMetadata(
        workspaceSceneMetadataWithResult(
          useStore.getState().selectedWorkspace()?._meta,
          "loading",
          `Loading impact for ${targetNodeId}.`,
        ),
      );

      const result = await loadImpactLens(
        targetNodeId,
        {
          depth: requestDepth,
          confidence: requestConfidence,
          workspaceId: requestWorkspaceId,
        },
        controller.signal,
      );
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
      // We aborted this request (superseded query or unmount) — stay silent.
      if (err instanceof DOMException && err.name === "AbortError") return;
      if (!isCurrentRequest()) return;

      if (err instanceof ImpactTimeoutError) {
        // The backend is likely still computing ranks for a freshly indexed
        // workspace; the debounced pagerank:recomputed SSE will retry us.
        setSceneMetadata(
          workspaceSceneMetadataWithResult(
            useStore.getState().selectedWorkspace()?._meta,
            "timed-out",
            "Impact timed out while ranks are computing. It will refresh automatically when ready.",
          ),
        );
        notify({
          kind: "warning",
          title: "Impact is taking longer than expected",
          message:
            "The graph may be computing ranks for a freshly indexed workspace. It will refresh automatically when ready.",
        });
        return;
      }

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
    impactConfidence,
    impactDepth,
    notify,
    ranksGeneration,
    selectedNodeId,
    setActiveLens,
    setGraphData,
    setSceneMetadata,
  ]);

  useEffect(() => {
    loadImpactData();
    return () => {
      requestIdRef.current += 1;
      abortRef.current?.abort();
    };
  }, [loadImpactData]);
}
