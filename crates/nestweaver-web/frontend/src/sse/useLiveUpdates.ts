import { useEffect, useRef } from "react";
import { useStore } from "../stores";
import { clearNodePreviews } from "../hooks/useNodePreview";
import { clearSymbolQueries } from "../api/symbolQuery";

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
    es.onerror = () => setSseConnected(false);

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
    const handleUpdate = () => { graphPending = true; scheduleRefresh(); };
    const handleRanksRecomputed = () => { ranksPending = true; scheduleRefresh(); };

    es.addEventListener("graph:updated", handleUpdate);
    es.addEventListener("pagerank:recomputed", handleRanksRecomputed);
    es.addEventListener("watcher:status", () =>
      setLastEventTimestamp(Date.now()),
    );
    es.addEventListener("full_refresh", handleUpdate);

    return () => {
      if (refreshTimer !== null) clearTimeout(refreshTimer);
      es.close();
      setSseConnected(false);
    };
  }, [setSseConnected, setLastEventTimestamp]);
}
