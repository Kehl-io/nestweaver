import { useCallback, useEffect, useRef, useState } from "react";
import { useStore } from "../stores";
import type { GraphMode } from "../api/types";
import type { ActiveLensState, RepresentationMode } from "../api/p1Types";
import type { AnalysisStateSnapshot } from "../stores/analysisSlice";
import type { DetailFocus } from "../stores/graphSlice";

interface NavState {
  seeds: string[];
  graphMode: GraphMode;
  activeWorkspaceId: string;
  activeLens: ActiveLensState;
  representationMode: RepresentationMode;
  selectedNodeId: string | null;
  selectedNodeKind: string | null;
  detailFocus: DetailFocus;
  analysis: AnalysisStateSnapshot;
}

const MAX_HISTORY = 50;

const historyState = {
  entries: [] as NavState[],
  index: -1,
  isNavigating: false,
};

const listeners = new Set<() => void>();

const emptyAnalysisSnapshot: AnalysisStateSnapshot = {
  flowTraceRoot: null,
  pathfindingActive: false,
  pathfindingFrom: null,
  pathfindingTo: null,
  pathRequestId: 0,
  pathResults: [],
  pathStatus: "idle",
  pathError: null,
  selectedPathIndex: 0,
  diffActive: false,
  diffState: {
    snapshotA: null,
    snapshotB: null,
    seedsA: [],
    seedsB: [],
  },
  gapItems: [],
  gapActive: false,
  relationshipResult: null,
  backlinkResult: null,
};

function emitChange() {
  listeners.forEach((listener) => listener());
}

function subscribe(listener: () => void) {
  listeners.add(listener);
  return () => {
    listeners.delete(listener);
  };
}

function statesEqual(left: NavState | undefined, right: NavState): boolean {
  if (!left) return false;
  return (
    JSON.stringify(left.seeds) === JSON.stringify(right.seeds) &&
    left.graphMode === right.graphMode &&
    left.activeWorkspaceId === right.activeWorkspaceId &&
    left.activeLens.lens === right.activeLens.lens &&
    left.activeLens.label === right.activeLens.label &&
    left.activeLens.targetUid === right.activeLens.targetUid &&
    left.activeLens.workspaceId === right.activeLens.workspaceId &&
    left.representationMode === right.representationMode &&
    left.selectedNodeId === right.selectedNodeId &&
    left.selectedNodeKind === right.selectedNodeKind &&
    left.detailFocus === right.detailFocus &&
    JSON.stringify(left.analysis) === JSON.stringify(right.analysis)
  );
}

function analysisSnapshotForState(
  state: ReturnType<typeof useStore.getState>,
): AnalysisStateSnapshot {
  const snapshot: AnalysisStateSnapshot = {
    ...emptyAnalysisSnapshot,
  };

  if (state.activeLens.lens === "trace" && state.flowTraceRoot) {
    snapshot.flowTraceRoot = state.flowTraceRoot;
  }

  if (state.activeLens.lens === "path") {
    snapshot.pathfindingActive = state.pathfindingActive;
    snapshot.pathfindingFrom = state.pathfindingFrom;
    snapshot.pathfindingTo = state.pathfindingTo;
    snapshot.pathRequestId = state.pathRequestId;
    snapshot.pathResults = [...state.pathResults];
    snapshot.pathStatus = state.pathStatus;
    snapshot.pathError = state.pathError;
    snapshot.selectedPathIndex = state.selectedPathIndex;
  }

  if (
    state.diffActive &&
    state.detailFocus === "analysis" &&
    state.activeLens.label.toLowerCase().startsWith("compare")
  ) {
    snapshot.diffActive = true;
    snapshot.diffState = {
      snapshotA: state.diffState.snapshotA,
      snapshotB: state.diffState.snapshotB,
      seedsA: [...state.diffState.seedsA],
      seedsB: [...state.diffState.seedsB],
    };
  }

  if (
    state.gapActive &&
    (state.activeLens.lens === "unsupported" ||
      state.activeLens.label.toLowerCase().includes("dead code"))
  ) {
    snapshot.gapActive = true;
    snapshot.gapItems = [...state.gapItems];
  }

  const lowerLabel = state.activeLens.label.toLowerCase();
  if (
    state.relationshipResult &&
    state.activeLens.lens === "search" &&
    (lowerLabel.startsWith("callers of") || lowerLabel.startsWith("callees of"))
  ) {
    snapshot.relationshipResult = {
      ...state.relationshipResult,
      rows: [...state.relationshipResult.rows],
    };
  }

  if (
    state.backlinkResult &&
    state.activeLens.lens === "rationale" &&
    lowerLabel.startsWith("backlinks for")
  ) {
    snapshot.backlinkResult = {
      ...state.backlinkResult,
      rows: [...state.backlinkResult.rows],
    };
  }

  return snapshot;
}

function currentEntry(): NavState {
  const state = useStore.getState();
  return {
    seeds: [...state.seeds],
    graphMode: state.graphMode,
    activeWorkspaceId: state.activeWorkspaceId,
    activeLens: { ...state.activeLens },
    representationMode: state.representationMode,
    selectedNodeId: state.selectedNodeId,
    selectedNodeKind: state.selectedNodeKind,
    detailFocus: state.detailFocus,
    analysis: analysisSnapshotForState(state),
  };
}

export function useNavigationHistory() {
  const setSeeds = useStore((s) => s.setSeeds);
  const setGraphMode = useStore((s) => s.setGraphMode);
  const setActiveWorkspaceId = useStore((s) => s.setActiveWorkspaceId);
  const setActiveLens = useStore((s) => s.setActiveLens);
  const setRepresentationMode = useStore((s) => s.setRepresentationMode);
  const setDetailFocus = useStore((s) => s.setDetailFocus);
  const selectNode = useStore((s) => s.selectNode);
  const restoreAnalysisState = useStore((s) => s.restoreAnalysisState);
  const [, setVersion] = useState(0);

  const pushState = useCallback(() => {
    if (historyState.isNavigating) return;
    const entry = currentEntry();
    if (statesEqual(historyState.entries[historyState.index], entry)) return;

    // Trim forward history
    historyState.entries = historyState.entries.slice(0, historyState.index + 1);
    historyState.entries.push(entry);
    if (historyState.entries.length > MAX_HISTORY) {
      historyState.entries.shift();
    } else {
      historyState.index += 1;
    }
    emitChange();
  }, []);

  const undo = useCallback(() => {
    if (historyState.index <= 0) return;
    historyState.isNavigating = true;
    historyState.index -= 1;
    const entry = historyState.entries[historyState.index];
    setActiveWorkspaceId(entry.activeWorkspaceId);
    setSeeds(entry.seeds);
    setGraphMode(entry.graphMode);
    setActiveLens(entry.activeLens);
    setRepresentationMode(entry.representationMode);
    selectNode(entry.selectedNodeId, entry.selectedNodeKind);
    setDetailFocus(entry.detailFocus);
    restoreAnalysisState(entry.analysis, entry.activeLens);
    historyState.isNavigating = false;
    emitChange();
  }, [
    restoreAnalysisState,
    selectNode,
    setActiveLens,
    setActiveWorkspaceId,
    setDetailFocus,
    setGraphMode,
    setRepresentationMode,
    setSeeds,
  ]);

  const redo = useCallback(() => {
    if (historyState.index >= historyState.entries.length - 1) return;
    historyState.isNavigating = true;
    historyState.index += 1;
    const entry = historyState.entries[historyState.index];
    setActiveWorkspaceId(entry.activeWorkspaceId);
    setSeeds(entry.seeds);
    setGraphMode(entry.graphMode);
    setActiveLens(entry.activeLens);
    setRepresentationMode(entry.representationMode);
    selectNode(entry.selectedNodeId, entry.selectedNodeKind);
    setDetailFocus(entry.detailFocus);
    restoreAnalysisState(entry.analysis, entry.activeLens);
    historyState.isNavigating = false;
    emitChange();
  }, [
    restoreAnalysisState,
    selectNode,
    setActiveLens,
    setActiveWorkspaceId,
    setDetailFocus,
    setGraphMode,
    setRepresentationMode,
    setSeeds,
  ]);

  // Auto-push when scene-defining state changes (unless we're navigating via undo/redo)
  const seeds = useStore((s) => s.seeds);
  const graphMode = useStore((s) => s.graphMode);
  const activeWorkspaceId = useStore((s) => s.activeWorkspaceId);
  const activeLens = useStore((s) => s.activeLens);
  const representationMode = useStore((s) => s.representationMode);
  const selectedNodeId = useStore((s) => s.selectedNodeId);
  const selectedNodeKind = useStore((s) => s.selectedNodeKind);
  const detailFocus = useStore((s) => s.detailFocus);
  const flowTraceRoot = useStore((s) => s.flowTraceRoot);
  const pathfindingActive = useStore((s) => s.pathfindingActive);
  const pathfindingFrom = useStore((s) => s.pathfindingFrom);
  const pathfindingTo = useStore((s) => s.pathfindingTo);
  const pathRequestId = useStore((s) => s.pathRequestId);
  const pathResults = useStore((s) => s.pathResults);
  const pathStatus = useStore((s) => s.pathStatus);
  const pathError = useStore((s) => s.pathError);
  const selectedPathIndex = useStore((s) => s.selectedPathIndex);
  const diffActive = useStore((s) => s.diffActive);
  const diffState = useStore((s) => s.diffState);
  const gapItems = useStore((s) => s.gapItems);
  const gapActive = useStore((s) => s.gapActive);
  const relationshipResult = useStore((s) => s.relationshipResult);
  const backlinkResult = useStore((s) => s.backlinkResult);
  const prevSeedsRef = useRef<string>(JSON.stringify(seeds));
  const prevModeRef = useRef<GraphMode>(graphMode);
  const prevWorkspaceRef = useRef(activeWorkspaceId);
  const prevLensRef = useRef(JSON.stringify(activeLens));
  const prevRepresentationRef = useRef<RepresentationMode>(representationMode);
  const prevSelectionRef = useRef(`${selectedNodeId ?? ""}\u0000${selectedNodeKind ?? ""}`);
  const prevDetailFocusRef = useRef<DetailFocus>(detailFocus);
  const prevAnalysisRef = useRef<string>(
    JSON.stringify(analysisSnapshotForState(useStore.getState())),
  );

  useEffect(() => subscribe(() => setVersion((version) => version + 1)), []);

  useEffect(() => {
    if (historyState.entries.length === 0) pushState();
  }, [pushState]);

  useEffect(() => {
    const seedsKey = JSON.stringify(seeds);
    const lensKey = JSON.stringify(activeLens);
    const selectionKey = `${selectedNodeId ?? ""}\u0000${selectedNodeKind ?? ""}`;
    const analysisKey = JSON.stringify(analysisSnapshotForState(useStore.getState()));
    if (
      seedsKey !== prevSeedsRef.current ||
      graphMode !== prevModeRef.current ||
      activeWorkspaceId !== prevWorkspaceRef.current ||
      lensKey !== prevLensRef.current ||
      representationMode !== prevRepresentationRef.current ||
      selectionKey !== prevSelectionRef.current ||
      detailFocus !== prevDetailFocusRef.current ||
      analysisKey !== prevAnalysisRef.current
    ) {
      prevSeedsRef.current = seedsKey;
      prevModeRef.current = graphMode;
      prevWorkspaceRef.current = activeWorkspaceId;
      prevLensRef.current = lensKey;
      prevRepresentationRef.current = representationMode;
      prevSelectionRef.current = selectionKey;
      prevDetailFocusRef.current = detailFocus;
      prevAnalysisRef.current = analysisKey;
      pushState();
    }
  }, [
    activeLens,
    activeWorkspaceId,
    detailFocus,
    diffActive,
    diffState,
    flowTraceRoot,
    gapActive,
    gapItems,
    graphMode,
    pathError,
    pathRequestId,
    pathResults,
    pathStatus,
    pathfindingActive,
    pathfindingFrom,
    pathfindingTo,
    pushState,
    representationMode,
    relationshipResult,
    seeds,
    selectedNodeId,
    selectedNodeKind,
    selectedPathIndex,
    backlinkResult,
  ]);

  const canUndo = historyState.index > 0;
  const canRedo = historyState.index < historyState.entries.length - 1;

  return { pushState, undo, redo, canUndo, canRedo };
}
