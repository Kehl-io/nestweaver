import type { StateCreator } from "zustand";
import type { ActiveLensState } from "../api/p1Types";
import type { StoreState } from "./index";
import type {
  BacklinkRow,
  BrainContextResult,
  FlowNode,
  PathResult,
  Symbol,
} from "../api/types";

export type { FlowNode } from "../api/types";

export interface DiffState {
  snapshotA: BrainContextResult | null;
  snapshotB: BrainContextResult | null;
  seedsA: string[];
  seedsB: string[];
}

export interface PathRequest {
  from: string;
  to: string;
  requestId: number;
}

export interface GapItem {
  type: "undocumented" | "untested" | "disconnected";
  label: string;
  detail: string;
  nodeUids: string[];
}

export type RelationshipResultKind = "callers" | "callees";

export interface RelationshipResultState {
  kind: RelationshipResultKind;
  targetUid: string;
  targetLabel: string;
  workspaceId: string;
  rows: Symbol[];
  status: "success" | "empty" | "error";
  error: string | null;
}

export interface BacklinkResultState {
  targetUid: string;
  targetLabel: string;
  workspaceId: string;
  rows: BacklinkRow[];
  status: "success" | "empty" | "error";
  error: string | null;
}

export interface AnalysisStateSnapshot {
  flowTraceRoot: FlowNode | null;
  pathfindingActive: boolean;
  pathfindingFrom: string | null;
  pathfindingTo: string | null;
  pathRequestId: number;
  pathResults: PathResult[];
  pathStatus: "idle" | "pending" | "success" | "empty" | "error";
  pathError: string | null;
  selectedPathIndex: number;
  diffActive: boolean;
  diffState: DiffState;
  gapItems: GapItem[];
  gapActive: boolean;
  relationshipResult: RelationshipResultState | null;
  backlinkResult: BacklinkResultState | null;
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
  pathRequestId: number;
  pathResults: PathResult[];
  pathStatus: "idle" | "pending" | "success" | "empty" | "error";
  pathError: string | null;
  selectedPathIndex: number;
  startPathfinding: (from: string) => void;
  setPathfindingTarget: (to: string) => PathRequest;
  setPathResults: (results: PathResult[], request?: PathRequest) => void;
  setPathError: (error: string, request?: PathRequest) => void;
  isCurrentPathRequest: (request: PathRequest) => boolean;
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

  relationshipResult: RelationshipResultState | null;
  setRelationshipResult: (result: RelationshipResultState | null) => void;
  backlinkResult: BacklinkResultState | null;
  setBacklinkResult: (result: BacklinkResultState | null) => void;
  restoreAnalysisState: (
    snapshot: AnalysisStateSnapshot,
    lens?: ActiveLensState,
  ) => void;
}

function flattenFlowTree(node: FlowNode): string[] {
  const uids: string[] = [node.uid];
  for (const child of node.children) {
    uids.push(...flattenFlowTree(child));
  }
  return uids;
}

function pathRequestMatches(state: StoreState, request?: PathRequest): boolean {
  if (!request) return true;
  return (
    state.pathfindingActive &&
    state.pathfindingFrom === request.from &&
    state.pathfindingTo === request.to &&
    state.pathRequestId === request.requestId
  );
}

function lensKeepsDiff(lens: ActiveLensState): boolean {
  return lens.label.toLowerCase().startsWith("compare");
}

function lensKeepsGap(lens: ActiveLensState): boolean {
  const label = lens.label.toLowerCase();
  return lens.lens === "unsupported" && (label.includes("dead code") || label.includes("gap"));
}

function lensKeepsRelationship(lens: ActiveLensState): boolean {
  const label = lens.label.toLowerCase();
  return lens.lens === "search" && (label.startsWith("callers of") || label.startsWith("callees of"));
}

function lensKeepsBacklinks(lens: ActiveLensState): boolean {
  return lens.lens === "rationale" && lens.label.toLowerCase().startsWith("backlinks for");
}

export const createAnalysisSlice: StateCreator<
  StoreState,
  [["zustand/immer", never]],
  [],
  AnalysisSlice
> = (set, get) => ({
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
  pathRequestId: 0,
  pathResults: [],
  pathStatus: "idle",
  pathError: null,
  selectedPathIndex: 0,

  startPathfinding: (from) =>
    set((s) => {
      s.pathfindingActive = true;
      s.pathfindingFrom = from;
      s.pathfindingTo = null;
      s.pathRequestId += 1;
      s.pathResults = [];
      s.pathStatus = "idle";
      s.pathError = null;
      s.selectedPathIndex = 0;
    }),

  setPathfindingTarget: (to) => {
    const requestId = get().pathRequestId + 1;
    const from = get().pathfindingFrom ?? "";
    set((s) => {
      s.pathfindingTo = to;
      s.pathRequestId = requestId;
      s.pathStatus = "pending";
      s.pathError = null;
    });
    return {
      from,
      to,
      requestId,
    };
  },

  setPathResults: (results, request) =>
    set((s) => {
      if (!pathRequestMatches(s, request)) return;
      s.pathResults = results;
      s.pathStatus = results.length > 0 ? "success" : "empty";
      s.pathError = null;
      s.selectedPathIndex = 0;
    }),

  setPathError: (error, request) =>
    set((s) => {
      if (!pathRequestMatches(s, request)) return;
      s.pathResults = [];
      s.pathStatus = "error";
      s.pathError = error;
      s.selectedPathIndex = 0;
    }),

  isCurrentPathRequest: (request) => pathRequestMatches(get(), request),

  selectPath: (index) =>
    set((s) => {
      s.selectedPathIndex = index;
    }),

  clearPathfinding: () =>
    set((s) => {
      s.pathfindingActive = false;
      s.pathfindingFrom = null;
      s.pathfindingTo = null;
      s.pathRequestId += 1;
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

  relationshipResult: null,

  setRelationshipResult: (result) =>
    set((s) => {
      s.relationshipResult = result;
    }),

  backlinkResult: null,

  setBacklinkResult: (result) =>
    set((s) => {
      s.backlinkResult = result;
    }),

  restoreAnalysisState: (snapshot, lens) =>
    set((s) => {
      const targetLens = lens ?? s.activeLens;
      const keepTrace = targetLens.lens === "trace";
      const keepPath = targetLens.lens === "path";
      const keepDiff = lensKeepsDiff(targetLens);
      const keepGap = lensKeepsGap(targetLens);
      const keepRelationship = lensKeepsRelationship(targetLens);
      const keepBacklinks = lensKeepsBacklinks(targetLens);

      s.flowTraceRoot = keepTrace ? snapshot.flowTraceRoot : null;
      s.flowTraceNodeUids = s.flowTraceRoot
        ? flattenFlowTree(s.flowTraceRoot!)
        : [];
      s.flowTraceActive = s.flowTraceRoot !== null;
      s.pathfindingActive = keepPath ? snapshot.pathfindingActive : false;
      s.pathfindingFrom = keepPath ? snapshot.pathfindingFrom : null;
      s.pathfindingTo = keepPath ? snapshot.pathfindingTo : null;
      s.pathRequestId = keepPath ? snapshot.pathRequestId : s.pathRequestId + 1;
      s.pathResults = keepPath ? snapshot.pathResults : [];
      s.pathStatus = keepPath ? snapshot.pathStatus : "idle";
      s.pathError = keepPath ? snapshot.pathError : null;
      s.selectedPathIndex = keepPath ? snapshot.selectedPathIndex : 0;
      s.diffActive = keepDiff ? snapshot.diffActive : false;
      s.diffState = {
        snapshotA: keepDiff ? snapshot.diffState.snapshotA : null,
        snapshotB: keepDiff ? snapshot.diffState.snapshotB : null,
        seedsA: keepDiff ? [...snapshot.diffState.seedsA] : [],
        seedsB: keepDiff ? [...snapshot.diffState.seedsB] : [],
      };
      s.gapItems = keepGap ? snapshot.gapItems : [];
      s.gapActive = keepGap ? snapshot.gapActive : false;
      s.relationshipResult = keepRelationship ? snapshot.relationshipResult : null;
      s.backlinkResult = keepBacklinks ? snapshot.backlinkResult : null;
    }),
});
