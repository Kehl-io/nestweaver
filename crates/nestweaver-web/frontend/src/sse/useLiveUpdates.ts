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

    const handleUpdate = () => {
      setLastEventTimestamp(Date.now());
      if (seedsRef.current.length > 0) {
        useStore.getState().setSeeds([...seedsRef.current]);
      }
    };

    es.addEventListener("graph:updated", handleUpdate);
    es.addEventListener("pagerank:recomputed", handleUpdate);
    es.addEventListener("watcher:status", () =>
      setLastEventTimestamp(Date.now()),
    );
    es.addEventListener("full_refresh", handleUpdate);

    return () => {
      es.close();
      setSseConnected(false);
    };
  }, [setSseConnected, setLastEventTimestamp]);
}
