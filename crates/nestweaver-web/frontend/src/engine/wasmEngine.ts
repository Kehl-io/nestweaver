import { get as idbGet, set as idbSet } from "idb-keyval";
import { createWasmBridge, type WasmBridge } from "./wasm-bridge";

/**
 * One process-wide WASM engine (nw-570).
 *
 * The engine choice is read once, when the module loads, so a later URL
 * rewrite cannot switch engines mid-session, and every consumer shares this
 * single bridge and snapshot. `start()` and `refresh()` are idempotent and
 * serialized, so the ~37 MB `snapshot.msgpack` is downloaded at most once per
 * graph generation (and not at all when IndexedDB already holds it).
 */

export type EngineMode = "server" | "wasm";
export type WasmEngineStatus = "off" | "loading" | "ready" | "error";

export interface WasmEngineState {
  mode: EngineMode;
  status: WasmEngineStatus;
  nodeCount: number;
  edgeCount: number;
  generation: number | null;
  error: string | null;
  /** Completed sync passes (initial load plus each refresh check). */
  syncs: number;
}

const SNAPSHOT_URL = "/api/v1/snapshot.msgpack";
const CACHE_KEY = "nestweaver-snapshot";

function initialMode(): EngineMode {
  if (typeof window === "undefined") return "server";
  return new URLSearchParams(window.location.search).get("engine") === "wasm"
    ? "wasm"
    : "server";
}

export const ENGINE_MODE: EngineMode = initialMode();

let state: WasmEngineState = {
  mode: ENGINE_MODE,
  status: ENGINE_MODE === "wasm" ? "loading" : "off",
  nodeCount: 0,
  edgeCount: 0,
  generation: null,
  error: null,
  syncs: 0,
};
const listeners = new Set<() => void>();
let bridge: WasmBridge | null = null;
// Serializes start/refresh so concurrent callers share one download.
let pending: Promise<void> | null = null;
// A refresh requested mid-download re-checks the generation once it lands.
let rerun = false;
let started = false;

function update(next: Partial<WasmEngineState>) {
  state = { ...state, ...next };
  listeners.forEach((listener) => listener());
}

export function subscribeWasmEngine(listener: () => void): () => void {
  listeners.add(listener);
  return () => listeners.delete(listener);
}

export function getWasmEngineState(): WasmEngineState {
  return state;
}

async function serverGeneration(): Promise<number | null> {
  const response = await fetch("/api/v1/version");
  if (!response.ok) return null;
  const body = (await response.json()) as { graph_generation?: number };
  return typeof body.graph_generation === "number" ? body.graph_generation : null;
}

async function snapshotFor(
  generation: number | null,
): Promise<{ data: ArrayBuffer; generation: number | null }> {
  if (generation !== null) {
    try {
      const cached = await idbGet<{ generation: number; data: ArrayBuffer }>(CACHE_KEY);
      if (cached && cached.generation === generation) {
        return { data: cached.data, generation };
      }
    } catch {
      // IndexedDB can be unavailable (private windows); fall through to fetch.
    }
  }
  const response = await fetch(SNAPSHOT_URL);
  if (!response.ok) {
    throw new Error(`Snapshot download failed: ${response.status}`);
  }
  const data = await response.arrayBuffer();
  const header = response.headers.get("X-Graph-Generation");
  const fetchedGeneration = header ? Number.parseInt(header, 10) : generation;
  if (fetchedGeneration !== null && Number.isFinite(fetchedGeneration)) {
    try {
      await idbSet(CACHE_KEY, { generation: fetchedGeneration, data: data.slice(0) });
    } catch {
      // Caching is an optimization only.
    }
  }
  return { data, generation: fetchedGeneration };
}

async function sync(): Promise<void> {
  if (!bridge) {
    const created = await createWasmBridge();
    if (!(await created.init())) {
      throw new Error("WASM module failed to initialize");
    }
    bridge = created;
  }
  const generation = await serverGeneration();
  // Loaded and the generation is unchanged, or unknowable (a non-OK
  // /version): keep the snapshot rather than re-download ~37 MB per event.
  if (state.status === "ready" && (generation === null || generation === state.generation)) {
    return;
  }
  const snapshot = await snapshotFor(generation);
  if (!(await bridge.loadSnapshot(snapshot.data))) {
    throw new Error("WASM engine could not load the snapshot");
  }
  const [nodeCount, edgeCount] = await Promise.all([bridge.nodeCount(), bridge.edgeCount()]);
  update({
    status: "ready",
    nodeCount,
    edgeCount,
    generation: snapshot.generation,
    error: null,
  });
}

function run(): Promise<void> {
  if (pending) {
    rerun = true;
    return pending;
  }
  pending = sync()
    .catch((error: unknown) => {
      const message = error instanceof Error ? error.message : String(error);
      console.warn("[wasmEngine]", message);
      update({ status: state.status === "ready" ? "ready" : "error", error: message });
    })
    .finally(() => {
      pending = null;
      update({ syncs: state.syncs + 1 });
      if (rerun) {
        rerun = false;
        void run();
      }
    });
  return pending;
}

/** Load the engine once; later calls are no-ops. */
export function startWasmEngine(): Promise<void> {
  if (ENGINE_MODE !== "wasm") return Promise.resolve();
  if (started) return Promise.resolve();
  started = true;
  return run();
}

/** Re-check the server generation after a graph change; downloads only if it moved. */
export function refreshWasmEngine(): Promise<void> {
  if (ENGINE_MODE !== "wasm" || !started) return Promise.resolve();
  return run();
}

export async function wasmPpr(
  seeds: string[],
  damping: number,
): Promise<Array<[string, number]> | null> {
  if (!bridge || state.status !== "ready") return null;
  return bridge.ppr(seeds, damping);
}
