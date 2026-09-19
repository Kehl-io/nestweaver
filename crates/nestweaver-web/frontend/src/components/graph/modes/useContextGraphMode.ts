import { useEffect, useRef, useState } from "react";
import { useStore } from "../../../stores";
import { api } from "../../../api/client";
import { useForceLayout } from "../../../hooks/useForceLayout";
import { buildGraphFromContext, finalizeNodeSizes } from "../utils/buildGraphFromContext";

export interface ContextGraphState {
  status: "idle" | "loading" | "ready" | "empty" | "error";
  message: string;
}

export function useContextGraphMode(mode: "local" | "features", seeds: string[]) {
  const graphMode = useStore((s) => s.graphMode);
  const workspaceId = useStore((s) => s.activeWorkspaceId);
  const seedRefresh = useStore((s) => s.seeds);
  const requestIdRef = useRef(0);
  const [state, setState] = useState<ContextGraphState>({ status: "idle", message: "" });
  const [revision, setRevision] = useState(0);
  const { start, stop, kill } = useForceLayout();
  const seedKey = JSON.stringify(seeds);

  useEffect(() => {
    if (graphMode !== mode) return;
    const requestId = ++requestIdRef.current;
    const controller = new AbortController();
    let stopTimer: ReturnType<typeof setTimeout> | undefined;
    const selectedSeeds: string[] = JSON.parse(seedKey);
    const label = mode === "local" ? "Local" : "Features";
    const current = () => {
      const store = useStore.getState();
      const liveSeeds = mode === "local" ? (store.selectedNodeId ? [store.selectedNodeId] : []) : store.seeds;
      return !controller.signal.aborted && requestId === requestIdRef.current &&
        store.graphMode === mode && store.activeWorkspaceId === workspaceId &&
        JSON.stringify(liveSeeds) === seedKey;
    };
    const store = useStore.getState();
    store.clearGraphData();
    store.setSceneMetadata(null);
    store.setActiveLens({ lens: "context", label, targetUid: selectedSeeds[0] ?? null, workspaceId });
    if (selectedSeeds.length === 0) {
      setState({ status: "empty", message: "Select a node and add it as a context seed to explore its stored relationships." });
    } else {
      setState({ status: "loading", message: `Loading ${label.toLowerCase()} relationships…` });
      void api.brainContext(selectedSeeds, null, "all", workspaceId, controller.signal).then((result) => {
        if (!current()) return;
        if (!Array.isArray(result.edges) || !result.graph_meta) {
          throw new Error("Relationship data is unavailable from this server. Update the daemon and retry.");
        }
        const graph = buildGraphFromContext(result);
        finalizeNodeSizes(graph);
        if (mode === "local" && graph.hasNode(selectedSeeds[0])) {
          graph.setNodeAttribute(selectedSeeds[0], "x", 0);
          graph.setNodeAttribute(selectedSeeds[0], "y", 0);
        }
        const active = useStore.getState();
        active.setGraphData(graph);
        active.setSceneMetadata(result._meta ?? null);
        const meta = result.graph_meta;
        const omissions = meta.truncated
          ? ` Limited result: ${meta.omitted_nodes} context nodes and ${meta.edge_count_relation === "gte" ? "at least " : ""}${meta.omitted_edges} relationships among returned nodes omitted.`
          : "";
        setState({
          status: graph.order === 0 ? "empty" : "ready",
          message: (graph.order === 0
            ? "No context nodes match this workspace."
            : graph.size === 0
              ? "No stored relationships were found among these context nodes."
              : `${graph.order} nodes · ${graph.size} stored relationships`) + omissions,
        });
        if (graph.order > 0) {
          start(graph);
          stopTimer = setTimeout(stop, 10_000);
        }
      }).catch((error: unknown) => {
        if (!current()) return;
        useStore.getState().clearGraphData();
        useStore.getState().setSceneMetadata(null);
        setState({ status: "error", message: error instanceof Error ? error.message : "Context relationships could not be loaded." });
      });
    }
    return () => {
      requestIdRef.current += 1;
      controller.abort();
      if (stopTimer) clearTimeout(stopTimer);
      kill();
    };
  }, [graphMode, mode, seedKey, seedRefresh, workspaceId, revision, start, stop, kill]);

  return { ...state, retry: () => setRevision((value) => value + 1) };
}
