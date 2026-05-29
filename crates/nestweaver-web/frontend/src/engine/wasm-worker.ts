import * as Comlink from "comlink";

interface WasmEngine {
  loadSnapshot(data: ArrayBuffer): Promise<void>;
  ppr(seeds: string[], damping: number): Promise<Array<[string, number]>>;
  nodeCount(): number;
  edgeCount(): number;
  generation(): number;
}

let engine: WasmEngine | null = null;

const api = {
  async init(): Promise<boolean> {
    try {
      // Dynamic import of the WASM module.
      // This will fail until the WASM is actually built and placed at the right path.
      // @ts-expect-error — WASM module not yet built
      const wasm = await import("../../wasm/nestweaver_wasm.js");
      await wasm.default(); // Initialize WASM
      engine = wasm as WasmEngine;
      return true;
    } catch (e) {
      console.warn("[wasm-worker] WASM module not available:", e);
      return false;
    }
  },

  async loadSnapshot(data: ArrayBuffer): Promise<boolean> {
    if (!engine) return false;
    try {
      await engine.loadSnapshot(data);
      return true;
    } catch (e) {
      console.error("[wasm-worker] Failed to load snapshot:", e);
      return false;
    }
  },

  async ppr(seeds: string[], damping: number): Promise<Array<[string, number]>> {
    if (!engine) return [];
    return engine.ppr(seeds, damping);
  },

  nodeCount(): number {
    return engine?.nodeCount() ?? 0;
  },

  edgeCount(): number {
    return engine?.edgeCount() ?? 0;
  },

  generation(): number {
    return engine?.generation() ?? 0;
  },
};

Comlink.expose(api);
