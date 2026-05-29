import type { StateCreator } from "zustand";
import type Graph from "graphology";
import type { StoreState } from "./index";

export interface GraphDataSlice {
  graphInstance: Graph | null;
  graphVersion: number;
  setGraphData: (graph: Graph) => void;
  clearGraphData: () => void;
}

export const createGraphDataSlice: StateCreator<
  StoreState,
  [["zustand/immer", never]],
  [],
  GraphDataSlice
> = (set) => ({
  graphInstance: null,
  graphVersion: 0,
  setGraphData: (graph) =>
    set((s) => {
      s.graphInstance = graph as any;
      s.graphVersion += 1;
    }),
  clearGraphData: () =>
    set((s) => {
      s.graphInstance = null;
      s.graphVersion += 1;
    }),
});
