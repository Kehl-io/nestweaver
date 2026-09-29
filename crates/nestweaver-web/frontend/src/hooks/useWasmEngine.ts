import { useEffect, useSyncExternalStore } from "react";
import { useStore } from "../stores";
import {
  getWasmEngineState,
  refreshWasmEngine,
  startWasmEngine,
  subscribeWasmEngine,
  type WasmEngineState,
} from "../engine/wasmEngine";

/** Read the shared engine state. Safe to call from any number of components. */
export function useWasmEngine(): WasmEngineState {
  return useSyncExternalStore(subscribeWasmEngine, getWasmEngineState);
}

/**
 * Drive the shared engine: load it once and re-check the snapshot generation
 * after live graph events. Mount exactly once (App), never per consumer.
 */
export function useWasmEngineLifecycle(): void {
  const lastEventTimestamp = useStore((s) => s.lastEventTimestamp);

  useEffect(() => {
    void startWasmEngine();
  }, []);

  useEffect(() => {
    if (lastEventTimestamp != null && lastEventTimestamp > 0) {
      void refreshWasmEngine();
    }
  }, [lastEventTimestamp]);
}
