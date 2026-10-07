import { useEffect, useRef } from "react";
import { useStore } from "../stores";
import { clearNodePreviews } from "../hooks/useNodePreview";
import { clearSymbolQueries } from "../api/symbolQuery";
import { establishInitialGraphBaseline, releaseInitialReadsWithoutBaseline } from "./initialReadBarrier";

export function useLiveUpdates() {
  const setSseConnected = useStore((s) => s.setSseConnected);
  const setLastEventTimestamp = useStore((s) => s.setLastEventTimestamp);
  const seedsRef = useRef(useStore.getState().seeds);

  // Keep ref in sync with store
  useEffect(() => {
    return useStore.subscribe((state) => {
      seedsRef.current = state.seeds;
    });
  }, []);

  useEffect(() => {
    const es = new EventSource("/api/v1/events");

    es.onopen = () => setSseConnected(true);
    es.onerror = () => {
      setSseConnected(false);
      releaseInitialReadsWithoutBaseline();
    };

    const refreshSeeds = () => {
      if (seedsRef.current.length > 0) {
        useStore.getState().setSeeds([...seedsRef.current]);
      }
    };

    // One quiet window publishes one committed epoch, independently of heartbeats.
    let refreshTimer: ReturnType<typeof setTimeout> | null = null;
    let graphPending = false;
    let ranksPending = false;
    const scheduleRefresh = () => {
      setLastEventTimestamp(Date.now());
      if (refreshTimer !== null) clearTimeout(refreshTimer);
      refreshTimer = setTimeout(() => {
        refreshTimer = null;
        const committed = graphPending;
        const ranked = ranksPending;
        graphPending = false;
        ranksPending = false;
        if (committed) {
          clearNodePreviews();
          clearSymbolQueries();
          useStore.getState().bumpGraphEpoch();
          void useStore.getState().loadWorkspaces();
        }
        if (ranked) {
          useStore.getState().bumpRanksGeneration();
          if (!committed) refreshSeeds();
        }
      }, 400);
    };
    let generation: string | null = null;
    let rankGeneration: string | null = null;
    const readGenerations = (event?: Event) => {
      if (!(event instanceof MessageEvent)) return null;
      try {
        const payload = JSON.parse(event.data) as { graph_generation?: unknown; pagerank_generation?: unknown };
        return typeof payload.graph_generation === "string" && /^\d+$/.test(payload.graph_generation) &&
          typeof payload.pagerank_generation === "string" && /^\d+$/.test(payload.pagerank_generation)
          ? { graph: payload.graph_generation, ranks: payload.pagerank_generation } : null;
      } catch { return null; }
    };
    const handleGeneration = (event: Event) => {
      const snapshot = readGenerations(event);
      if (!snapshot) return;
      if (generation === null) {
        // A verified snapshot releases initial reads without duplicating them.
        // Error/timeout fallback reads need one catch-up when SSE returns.
        generation = snapshot.graph;
        rankGeneration = snapshot.ranks;
        if (establishInitialGraphBaseline()) {
          graphPending = true;
          ranksPending = true;
          scheduleRefresh();
        }
        return;
      }
      const graphChanged = generation !== snapshot.graph;
      const ranksChanged = rankGeneration !== snapshot.ranks;
      generation = snapshot.graph;
      rankGeneration = snapshot.ranks;
      graphPending ||= graphChanged;
      ranksPending ||= ranksChanged;
      if (graphChanged || ranksChanged) scheduleRefresh();
    };
    const handleUpdate = (event: Event) => {
      handleGeneration(event);
      graphPending = true;
      scheduleRefresh();
    };
    const handleRanksRecomputed = (event: Event) => {
      handleGeneration(event);
      ranksPending = true;
      scheduleRefresh();
    };

    es.addEventListener("graph:generation", handleGeneration);
    es.addEventListener("graph:updated", handleUpdate);
    es.addEventListener("pagerank:recomputed", handleRanksRecomputed);
    es.addEventListener("watcher:status", () =>
      setLastEventTimestamp(Date.now()),
    );
    es.addEventListener("full_refresh", (event) => {
      handleUpdate(event);
      ranksPending = true;
    });

    return () => {
      if (refreshTimer !== null) clearTimeout(refreshTimer);
      es.close();
      setSseConnected(false);
    };
  }, [setSseConnected, setLastEventTimestamp]);
}
