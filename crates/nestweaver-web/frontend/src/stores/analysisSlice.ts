import type { StateCreator } from "zustand";
import type { StoreState } from "./index";
import type { BrainContextResult, FlowNode, PathResult } from "../api/types";

export type { FlowNode } from "../api/types";

export interface DiffState {
  snapshotA: BrainContextResult | null;
  snapshotB: BrainContextResult | null;
  seedsA: string[];
  seedsB: string[];
}

export interface GapItem {
  type: "undocumented" | "untested" | "disconnected";
  label: string;
  detail: string;
  nodeUids: string[];
}

export interface AnalysisSlice {
  flowTraceRoot: FlowNode | null;
  flowTraceNodeUids: string[];
  flowTraceActive: boolean;
  setFlowTrace: (root: FlowNode | null) => void;
  clearFlowTrace: () => void;

  pathfindingActive: boolean;
  pathfindingFrom: string | null;
  pathfindingTo: string | null;
  pathResults: PathResult[];
  pathStatus: "idle" | "pending" | "success" | "empty" | "error";
  pathError: string | null;
  selectedPathIndex: number;
  startPathfinding: (from: string) => void;
  setPathfindingTarget: (to: string) => void;
  setPathResults: (results: PathResult[]) => void;
  setPathError: (error: string) => void;
  selectPath: (index: number) => void;
  clearPathfinding: () => void;

  diffActive: boolean;
  diffState: DiffState;
  startDiff: (snapshotA: BrainContextResult, seedsA: string[]) => void;
  setDiffB: (snapshotB: BrainContextResult, seedsB: string[]) => void;
  clearDiff: () => void;

  gapItems: GapItem[];
  gapActive: boolean;
  setGapItems: (items: GapItem[]) => void;
  toggleGapPanel: () => void;
}

function flattenFlowTree(node: FlowNode): string[] {
  const uids: string[] = [node.uid];
  for (const child of node.children) {
    uids.push(...flattenFlowTree(child));
  }
  return uids;
}

export const createAnalysisSlice: StateCreator<
  StoreState,
  [["zustand/immer", never]],
  [],
  AnalysisSlice
> = (set) => ({
  flowTraceRoot: null,
  flowTraceNodeUids: [],
  flowTraceActive: false,

  setFlowTrace: (root) =>
    set((s) => {
      s.flowTraceRoot = root;
      s.flowTraceNodeUids = root ? flattenFlowTree(root) : [];
      s.flowTraceActive = root !== null;
    }),

  clearFlowTrace: () =>
    set((s) => {
      s.flowTraceRoot = null;
      s.flowTraceNodeUids = [];
      s.flowTraceActive = false;
    }),

  pathfindingActive: false,
  pathfindingFrom: null,
  pathfindingTo: null,
  pathResults: [],
  pathStatus: "idle",
  pathError: null,
  selectedPathIndex: 0,

  startPathfinding: (from) =>
    set((s) => {
      s.pathfindingActive = true;
      s.pathfindingFrom = from;
      s.pathfindingTo = null;
      s.pathResults = [];
      s.pathStatus = "idle";
      s.pathError = null;
      s.selectedPathIndex = 0;
    }),

  setPathfindingTarget: (to) =>
    set((s) => {
      s.pathfindingTo = to;
      s.pathStatus = "pending";
      s.pathError = null;
    }),

  setPathResults: (results) =>
    set((s) => {
      s.pathResults = results;
      s.pathStatus = results.length > 0 ? "success" : "empty";
      s.pathError = null;
      s.selectedPathIndex = 0;
    }),

  setPathError: (error) =>
    set((s) => {
      s.pathResults = [];
      s.pathStatus = "error";
      s.pathError = error;
      s.selectedPathIndex = 0;
    }),

  selectPath: (index) =>
    set((s) => {
      s.selectedPathIndex = index;
    }),

  clearPathfinding: () =>
    set((s) => {
      s.pathfindingActive = false;
      s.pathfindingFrom = null;
      s.pathfindingTo = null;
      s.pathResults = [];
      s.pathStatus = "idle";
      s.pathError = null;
      s.selectedPathIndex = 0;
    }),

  diffActive: false,
  diffState: {
    snapshotA: null,
    snapshotB: null,
    seedsA: [],
    seedsB: [],
  },

  startDiff: (snapshotA, seedsA) =>
    set((s) => {
      s.diffActive = true;
      s.diffState.snapshotA = snapshotA;
      s.diffState.seedsA = seedsA;
      s.diffState.snapshotB = null;
      s.diffState.seedsB = [];
    }),

  setDiffB: (snapshotB, seedsB) =>
    set((s) => {
      s.diffState.snapshotB = snapshotB;
      s.diffState.seedsB = seedsB;
    }),

  clearDiff: () =>
    set((s) => {
      s.diffActive = false;
      s.diffState.snapshotA = null;
      s.diffState.snapshotB = null;
      s.diffState.seedsA = [];
      s.diffState.seedsB = [];
    }),

  gapItems: [],
  gapActive: false,

  setGapItems: (items) =>
    set((s) => {
      s.gapItems = items;
    }),

  toggleGapPanel: () =>
    set((s) => {
      s.gapActive = !s.gapActive;
    }),
});
