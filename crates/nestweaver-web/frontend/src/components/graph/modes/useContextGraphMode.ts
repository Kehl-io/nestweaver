import { useEffect, useRef, useState } from "react";
import type Graph from "graphology";
import { preserveGraphLayout } from "../utils/preserveGraphLayout";
import { useStore } from "../../../stores";
import { api } from "../../../api/client";
import { useForceLayout } from "../../../hooks/useForceLayout";
import { buildGraphFromContext, finalizeNodeSizes } from "../utils/buildGraphFromContext";

export interface ContextGraphState {
  status: "idle" | "loading" | "ready" | "empty" | "error";
  message: string;
}

function compareOwnsLens(): boolean {
  const state = useStore.getState();
  return state.diffActive || state.activeLens.label.toLowerCase().startsWith("compare");
}

export function useContextGraphMode(mode: "local" | "features", seeds: string[]) {
  const graphMode = useStore((s) => s.graphMode);
  const graphEpoch = useStore((s) => s.graphEpoch);
  const workspaceId = useStore((s) => s.activeWorkspaceId);
  const seedRefresh = useStore((s) => s.seeds);
  const previousLayout = useRef<{ key: string; graph: Graph; interrupted?: boolean } | null>(null);
  const requestIdRef = useRef(0);
  const [state, setState] = useState<ContextGraphState>({ status: "idle", message: "" });
  const [revision, setRevision] = useState(0);
  const { start, stop, kill } = useForceLayout();
  const seedKey = JSON.stringify(seeds);

  useEffect(() => {
    if (graphMode !== mode) return;
    // Compare owns the lens. Reloading Local/Features (including a force-param
    // `start` identity change) must not select this mode and clear that analysis.
    if (compareOwnsLens()) return;
    const requestId = ++requestIdRef.current;
    const controller = new AbortController();
    let stopTimer: ReturnType<typeof setTimeout> | undefined;
    const selectedSeeds: string[] = JSON.parse(seedKey);
    const label = mode === "local" ? "Local" : "Features";
    const current = () => {
      const store = useStore.getState();
      const localTarget = store.inspectedSceneTarget?.mode === "local" &&
        store.inspectedSceneTarget.workspaceId === store.activeWorkspaceId
        ? store.inspectedSceneTarget.uid : store.selectedNodeId;
      const liveSeeds = mode === "local" ? (localTarget ? [localTarget] : []) : store.seeds;
      return !controller.signal.aborted && requestId === requestIdRef.current &&
        store.graphMode === mode && store.activeWorkspaceId === workspaceId && store.graphEpoch === graphEpoch &&
        JSON.stringify(liveSeeds) === seedKey;
    };
    const store = useStore.getState();

    const layoutKey = `${mode}:${workspaceId}:${seedKey}`;
    const existingScene = previousLayout.current?.key === layoutKey &&
      previousLayout.current.graph === store.graphInstance;
    if (!existingScene) {
      store.clearGraphData();
      store.setSceneMetadata(null);
      store.setActiveLens({ lens: "context", label, targetUid: selectedSeeds[0] ?? null, workspaceId });
    }
    if (selectedSeeds.length === 0) {
      setState({
        status: "empty",
        message: mode === "local"
          ? "Select a node to explore its stored relationships."
          : "Add a context seed to explore its stored relationships.",
      });
    } else {
      setState({ status: "loading", message: `Loading ${label.toLowerCase()} relationships…` });
      void api.brainContext(selectedSeeds, null, "all", workspaceId, controller.signal).then((result) => {
        if (!current() || compareOwnsLens()) return;
        if (!Array.isArray(result.edges) || !result.graph_meta) {
          throw new Error("Relationship data is unavailable from this server. Update the daemon and retry.");
        }
        const graph = buildGraphFromContext(result);
        finalizeNodeSizes(graph);
        if (mode === "local" && graph.hasNode(selectedSeeds[0])) {
          graph.setNodeAttribute(selectedSeeds[0], "x", 0);
          graph.setNodeAttribute(selectedSeeds[0], "y", 0);
        }
        preserveGraphLayout(graph, existingScene ? previousLayout.current?.graph ?? null : null);
        previousLayout.current = { key: layoutKey, graph, interrupted: previousLayout.current?.interrupted };
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
        if (graph.order > 0 && (!existingScene || previousLayout.current?.interrupted) && !active.reducedEffects) {
          if (previousLayout.current) previousLayout.current.interrupted = false;
          start(graph);
          stopTimer = setTimeout(stop, 10_000);
        }
      }).catch((error: unknown) => {
        if (!current() || compareOwnsLens()) return;

        setState({ status: "error", message: error instanceof Error ? error.message : "Context relationships could not be loaded." });
      });
    }
    return () => {
      requestIdRef.current += 1;
      controller.abort();
      if (stopTimer) clearTimeout(stopTimer);
      const interrupted = kill();
      if (previousLayout.current) previousLayout.current.interrupted ||= interrupted;
    };
  }, [graphMode, mode, seedKey, seedRefresh, workspaceId, graphEpoch, revision, start, stop, kill]);

  return { ...state, retry: () => setRevision((value) => value + 1) };
}
