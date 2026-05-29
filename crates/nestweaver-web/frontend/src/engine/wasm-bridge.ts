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
  loadSnapshot(data: ArrayBuffer): boolean;
  ppr(seedsJson: string, damping: number): string;
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

  const remote = Comlink.wrap<WorkerApi>(worker);

  const bridge: WasmBridge = {
    init: () => remote.init(),
    loadSnapshot: (data) => remote.loadSnapshot(Comlink.transfer(data, [data])),
    async ppr(seeds: string[], damping: number): Promise<Array<[string, number]>> {
      const json = await remote.ppr(JSON.stringify(seeds), damping);
      try {
        return JSON.parse(json);
      } catch {
        return [];
      }
    },
    nodeCount: () => remote.nodeCount(),
    edgeCount: () => remote.edgeCount(),
    generation: () => remote.generation(),
    dispose: () => {
      worker.terminate();
      instance = null;
    },
  };

  instance = bridge;
  return bridge;
}
