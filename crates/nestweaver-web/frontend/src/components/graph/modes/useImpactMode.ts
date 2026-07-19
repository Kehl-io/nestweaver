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
  // Full query identity of the last load we kicked off. A background rank
  // refresh (SSE pagerank:recomputed -> ranksGeneration bump) re-runs this hook
  // with the SAME key, so we can refetch silently instead of flashing to the
  // loading scene (nw-029). Key MUST include depth + confidence, not just
  // workspace/target — a partial key was the original pre-T6 hang bug.
  const queryKeyRef = useRef<string | null>(null);
  const previousLayoutRef = useRef<{ key: string; graph: Graph } | null>(
    null,
  );

  const loadImpactData = useCallback(async () => {
    // `ranksGeneration` is an invalidation token: the graph query itself does
    // not send it, but a new generation must rerun this callback.
    void ranksGeneration;
    // Any prior in-flight request is now superseded — abort its fetch so we
    // don't leave a hung PageRank request running against a cold DB.
    abortRef.current?.abort();

    if (graphMode !== "impact" || !selectedNodeId) {
      requestIdRef.current += 1;
      abortRef.current = null;
      // Leaving impact mode: forget the last query so re-entering always reads
      // as a new query and shows loading for its first fetch.
      queryKeyRef.current = null;
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
    // Full query identity — a change in any of these is a genuinely new query.
    const queryKey = `${requestWorkspaceId}:${targetNodeId}:${requestDepth}:${requestConfidence}`;
    const isNewQuery = queryKeyRef.current !== queryKey;
    queryKeyRef.current = queryKey;
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
      // Show the loading scene for a genuinely new query, or when nothing is
      // currently rendered. A background rank refresh of an already-displayed
      // graph (same query key, graph present) refetches silently and swaps in,
      // so steady-state reindexing doesn't blink a healthy graph to a spinner
      // every debounce window (nw-029). We still clear+load on every *new*
      // query (workspace/target/depth/confidence change), keeping the honest
      // loading state and the pre-T6 depth/confidence-change fix intact.
      const hasGraph = (useStore.getState().graphInstance?.order ?? 0) > 0;
      if (isNewQuery || !hasGraph) {
        clearGraphData();
        setSceneMetadata(
          workspaceSceneMetadataWithResult(
            useStore.getState().selectedWorkspace()?._meta,
            "loading",
            `Loading impact for ${targetNodeId}.`,
          ),
        );
      }

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
