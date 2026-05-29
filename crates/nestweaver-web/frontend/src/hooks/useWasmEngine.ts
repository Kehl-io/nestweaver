import { useEffect, useRef, useState } from "react";
import { useEngineMode } from "./useEngineMode";
import { useSnapshotSync } from "./useSnapshotSync";
import { createWasmBridge, type WasmBridge } from "../engine/wasm-bridge";

interface WasmEngineState {
  enabled: boolean;
  ready: boolean;
  nodeCount: number;
  edgeCount: number;
  bridge: WasmBridge | null;
  ppr: (seeds: string[], damping: number) => Promise<Array<[string, number]> | null>;
}

export function useWasmEngine(): WasmEngineState {
  const mode = useEngineMode();
  const enabled = mode === "wasm";
  const sync = useSnapshotSync(enabled);
  const bridgeRef = useRef<WasmBridge | null>(null);
  const [ready, setReady] = useState(false);
  const [nodeCount, setNodeCount] = useState(0);
  const [edgeCount, setEdgeCount] = useState(0);

  useEffect(() => {
    if (!enabled) return;

    let cancelled = false;

    (async () => {
      try {
        const bridge = await createWasmBridge();
        const initialized = await bridge.init();
        if (!initialized || cancelled) return;
        bridgeRef.current = bridge;

        // Download and load snapshot
        const resp = await fetch("/api/v1/snapshot.msgpack");
        if (!resp.ok || cancelled) return;
        const data = await resp.arrayBuffer();
        const loaded = await bridge.loadSnapshot(data);
        if (loaded && !cancelled) {
          const nodes = await bridge.nodeCount();
          const edges = await bridge.edgeCount();
          setNodeCount(nodes);
          setEdgeCount(edges);
          setReady(true);
          console.log(
            "[useWasmEngine] WASM engine ready, nodes:",
            nodes,
            "edges:",
            edges,
          );
        }
      } catch (e) {
        console.warn("[useWasmEngine] Failed to initialize:", e);
      }
    })();

    return () => {
      cancelled = true;
    };
  }, [enabled]);

  // Re-load snapshot when generation changes
  useEffect(() => {
    if (!enabled || !ready || !sync.generation || !bridgeRef.current) return;

    let cancelled = false;

    (async () => {
      try {
        const resp = await fetch("/api/v1/snapshot.msgpack");
        if (!resp.ok || cancelled) return;
        const data = await resp.arrayBuffer();
        const bridge = bridgeRef.current;
        if (!bridge || cancelled) return;
        await bridge.loadSnapshot(data);
        const nodes = await bridge.nodeCount();
        const edges = await bridge.edgeCount();
        if (!cancelled) {
          setNodeCount(nodes);
          setEdgeCount(edges);
          console.log(
            "[useWasmEngine] Snapshot refreshed, nodes:",
            nodes,
            "edges:",
            edges,
          );
        }
      } catch (e) {
        console.warn("[useWasmEngine] Snapshot refresh failed:", e);
      }
    })();

    return () => {
      cancelled = true;
    };
  }, [enabled, ready, sync.generation]);

  return {
    enabled,
    ready,
    nodeCount,
    edgeCount,
    bridge: bridgeRef.current,
    async ppr(seeds: string[], damping: number) {
      if (!bridgeRef.current || !ready) return null;
      return bridgeRef.current.ppr(seeds, damping);
    },
  };
}
