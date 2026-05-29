import * as Comlink from "comlink";
import init, { WasmGraph } from "../wasm/nestweaver_wasm.js";

let graph: WasmGraph | null = null;
let initialized = false;

const api = {
  async init(): Promise<boolean> {
    if (initialized) return true;
    try {
      await init();
      initialized = true;
      return true;
    } catch (e) {
      console.warn("[wasm-worker] WASM init failed:", e);
      return false;
    }
  },

  loadSnapshot(data: ArrayBuffer): boolean {
    if (!initialized) return false;
    try {
      if (graph) {
        graph.free();
        graph = null;
      }
      graph = new WasmGraph(new Uint8Array(data));
      return true;
    } catch (e) {
      console.error("[wasm-worker] Failed to load snapshot:", e);
      return false;
    }
  },

  ppr(seedsJson: string, damping: number): string {
    if (!graph) return "[]";
    return graph.ppr(seedsJson, damping);
  },

  nodeCount(): number {
    return graph?.node_count() ?? 0;
  },

  edgeCount(): number {
    return graph?.edge_count() ?? 0;
  },

  generation(): number {
    return Number(graph?.generation() ?? 0n);
  },
};

Comlink.expose(api);
