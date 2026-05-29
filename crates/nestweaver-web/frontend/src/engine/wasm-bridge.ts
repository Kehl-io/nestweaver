import * as Comlink from "comlink";

export interface WasmBridge {
  init(): Promise<boolean>;
  loadSnapshot(data: ArrayBuffer): Promise<boolean>;
  ppr(seeds: string[], damping: number): Promise<Array<[string, number]>>;
  nodeCount(): Promise<number>;
  edgeCount(): Promise<number>;
  generation(): Promise<number>;
  dispose(): void;
}

type WorkerApi = {
  init(): Promise<boolean>;
  loadSnapshot(data: ArrayBuffer): Promise<boolean>;
  ppr(seeds: string[], damping: number): Promise<Array<[string, number]>>;
  nodeCount(): number;
  edgeCount(): number;
  generation(): number;
};

let instance: WasmBridge | null = null;

export async function createWasmBridge(): Promise<WasmBridge> {
  if (instance) return instance;

  const worker = new Worker(
    new URL("./wasm-worker.ts", import.meta.url),
    { type: "module" },
  );

  const api = Comlink.wrap<WorkerApi>(worker);

  const bridge: WasmBridge = {
    init: () => api.init(),
    loadSnapshot: (data) => api.loadSnapshot(Comlink.transfer(data, [data])),
    ppr: (seeds, damping) => api.ppr(seeds, damping),
    nodeCount: () => api.nodeCount(),
    edgeCount: () => api.edgeCount(),
    generation: () => api.generation(),
    dispose: () => {
      worker.terminate();
      instance = null;
    },
  };

  instance = bridge;
  return bridge;
}
