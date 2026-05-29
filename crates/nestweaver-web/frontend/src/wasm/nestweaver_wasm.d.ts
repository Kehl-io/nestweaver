/* tslint:disable */
/* eslint-disable */

/**
 * A graph loaded from MessagePack bytes, ready for in-browser algorithm execution.
 */
export class WasmGraph {
    free(): void;
    [Symbol.dispose](): void;
    /**
     * Get edge count.
     */
    edge_count(): number;
    /**
     * Get the graph generation number.
     */
    generation(): bigint;
    /**
     * Deserialize a graph from MessagePack bytes.
     */
    constructor(data: Uint8Array);
    /**
     * Get node count.
     */
    node_count(): number;
    /**
     * Run PPR with given seed UIDs (JSON array of strings) and return results as JSON.
     *
     * Returns a JSON array of `[uid, score]` pairs sorted descending by score.
     */
    ppr(seeds_json: string, damping: number): string;
}

export type InitInput = RequestInfo | URL | Response | BufferSource | WebAssembly.Module;

export interface InitOutput {
    readonly memory: WebAssembly.Memory;
    readonly __wbg_wasmgraph_free: (a: number, b: number) => void;
    readonly wasmgraph_edge_count: (a: number) => number;
    readonly wasmgraph_generation: (a: number) => bigint;
    readonly wasmgraph_new: (a: number, b: number) => [number, number, number];
    readonly wasmgraph_node_count: (a: number) => number;
    readonly wasmgraph_ppr: (a: number, b: number, c: number, d: number) => [number, number, number, number];
    readonly __wbindgen_externrefs: WebAssembly.Table;
    readonly __wbindgen_malloc: (a: number, b: number) => number;
    readonly __externref_table_dealloc: (a: number) => void;
    readonly __wbindgen_realloc: (a: number, b: number, c: number, d: number) => number;
    readonly __wbindgen_free: (a: number, b: number, c: number) => void;
    readonly __wbindgen_start: () => void;
}

export type SyncInitInput = BufferSource | WebAssembly.Module;

/**
 * Instantiates the given `module`, which can either be bytes or
 * a precompiled `WebAssembly.Module`.
 *
 * @param {{ module: SyncInitInput }} module - Passing `SyncInitInput` directly is deprecated.
 *
 * @returns {InitOutput}
 */
export function initSync(module: { module: SyncInitInput } | SyncInitInput): InitOutput;

/**
 * If `module_or_path` is {RequestInfo} or {URL}, makes a request and
 * for everything else, calls `WebAssembly.instantiate` directly.
 *
 * @param {{ module_or_path: InitInput | Promise<InitInput> }} module_or_path - Passing `InitInput` directly is deprecated.
 *
 * @returns {Promise<InitOutput>}
 */
export default function __wbg_init (module_or_path?: { module_or_path: InitInput | Promise<InitInput> } | InitInput | Promise<InitInput>): Promise<InitOutput>;
