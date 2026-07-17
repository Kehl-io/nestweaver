import { useEffect, useRef } from "react";
import { useStore } from "../stores";

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

    const handleUpdate = () => {
      setLastEventTimestamp(Date.now());
      refreshSeeds();
    };

    // A cold-start burst can emit one `pagerank:recomputed` per concurrent
    // request (nw-029 T4/T5). Coalesce them into a single refresh so we don't
    // fire N refetches: debounce the seed refresh and bump the ranks
    // generation once per quiet window, which lets a timed-out impact retry.
    let ranksTimer: ReturnType<typeof setTimeout> | null = null;
    const handleRanksRecomputed = () => {
      setLastEventTimestamp(Date.now());
      if (ranksTimer !== null) clearTimeout(ranksTimer);
      ranksTimer = setTimeout(() => {
        ranksTimer = null;
        useStore.getState().bumpRanksGeneration();
        refreshSeeds();
      }, 400);
    };

    es.addEventListener("graph:updated", handleUpdate);
    es.addEventListener("pagerank:recomputed", handleRanksRecomputed);
    es.addEventListener("watcher:status", () =>
      setLastEventTimestamp(Date.now()),
    );
    es.addEventListener("full_refresh", handleUpdate);

    return () => {
      if (ranksTimer !== null) clearTimeout(ranksTimer);
      es.close();
      setSseConnected(false);
    };
  }, [setSseConnected, setLastEventTimestamp]);
}
