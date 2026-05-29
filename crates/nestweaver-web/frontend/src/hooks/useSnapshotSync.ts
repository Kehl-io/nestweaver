import { useEffect, useRef, useCallback, useState } from "react";
import { useStore } from "../stores";
import { get as idbGet, set as idbSet } from "idb-keyval";

interface SnapshotSyncState {
  isLoading: boolean;
  generation: number | null;
  error: string | null;
}

export function useSnapshotSync(enabled: boolean): SnapshotSyncState {
  const lastEventTimestamp = useStore((s) => s.lastEventTimestamp);
  const [state, setState] = useState<SnapshotSyncState>({
    isLoading: false,
    generation: null,
    error: null,
  });
  const currentGenRef = useRef<number | null>(null);

  const checkAndSync = useCallback(async () => {
    if (!enabled) return;

    try {
      // Check server version
      const resp = await fetch("/api/v1/version");
      if (!resp.ok) return;
      const { graph_generation } = (await resp.json()) as { graph_generation: number };

      // Skip if generation hasn't changed
      if (graph_generation === currentGenRef.current) return;

      setState((s) => ({ ...s, isLoading: true, error: null }));

      // Try loading from IndexedDB cache first
      const cached = await idbGet<{ generation: number; data: ArrayBuffer }>("nestweaver-snapshot");
      if (cached && cached.generation === graph_generation) {
        currentGenRef.current = graph_generation;
        setState({ isLoading: false, generation: graph_generation, error: null });
        return;
      }

      // Download fresh snapshot
      const snapshotResp = await fetch("/api/v1/snapshot.msgpack");
      if (!snapshotResp.ok) {
        throw new Error(`Snapshot download failed: ${snapshotResp.status}`);
      }

      const data = await snapshotResp.arrayBuffer();
      const serverGen = parseInt(
        snapshotResp.headers.get("X-Graph-Generation") ?? "0",
        10,
      );

      // Cache in IndexedDB
      await idbSet("nestweaver-snapshot", { generation: serverGen, data: data.slice(0) });

      currentGenRef.current = serverGen;
      setState({ isLoading: false, generation: serverGen, error: null });
    } catch (e) {
      setState((s) => ({ ...s, isLoading: false, error: String(e) }));
    }
  }, [enabled]);

  // Check on SSE events
  useEffect(() => {
    if (enabled && lastEventTimestamp != null && lastEventTimestamp > 0) {
      checkAndSync();
    }
  }, [enabled, lastEventTimestamp, checkAndSync]);

  // Initial check on mount
  useEffect(() => {
    if (enabled) {
      checkAndSync();
    }
  }, [enabled, checkAndSync]);

  return state;
}
