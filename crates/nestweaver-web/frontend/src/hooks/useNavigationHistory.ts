import { useCallback, useEffect, useRef, useState } from "react";
import { useStore } from "../stores";
import type { GraphMode } from "../api/types";
import type { ActiveLensState, RepresentationMode } from "../api/p1Types";

interface NavState {
  seeds: string[];
  graphMode: GraphMode;
  activeWorkspaceId: string;
  activeLens: ActiveLensState;
  representationMode: RepresentationMode;
  selectedNodeId: string | null;
  selectedNodeKind: string | null;
}

const MAX_HISTORY = 50;

const historyState = {
  entries: [] as NavState[],
  index: -1,
  isNavigating: false,
};

const listeners = new Set<() => void>();

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
    left.selectedNodeKind === right.selectedNodeKind
  );
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
  };
}

export function useNavigationHistory() {
  const setSeeds = useStore((s) => s.setSeeds);
  const setGraphMode = useStore((s) => s.setGraphMode);
  const setActiveWorkspaceId = useStore((s) => s.setActiveWorkspaceId);
  const setActiveLens = useStore((s) => s.setActiveLens);
  const setRepresentationMode = useStore((s) => s.setRepresentationMode);
  const selectNode = useStore((s) => s.selectNode);
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
    historyState.isNavigating = false;
    emitChange();
  }, [
    selectNode,
    setActiveLens,
    setActiveWorkspaceId,
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
    historyState.isNavigating = false;
    emitChange();
  }, [
    selectNode,
    setActiveLens,
    setActiveWorkspaceId,
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
  const prevSeedsRef = useRef<string>(JSON.stringify(seeds));
  const prevModeRef = useRef<GraphMode>(graphMode);
  const prevWorkspaceRef = useRef(activeWorkspaceId);
  const prevLensRef = useRef(JSON.stringify(activeLens));
  const prevRepresentationRef = useRef<RepresentationMode>(representationMode);
  const prevSelectionRef = useRef(`${selectedNodeId ?? ""}\u0000${selectedNodeKind ?? ""}`);

  useEffect(() => subscribe(() => setVersion((version) => version + 1)), []);

  useEffect(() => {
    if (historyState.entries.length === 0) pushState();
  }, [pushState]);

  useEffect(() => {
    const seedsKey = JSON.stringify(seeds);
    const lensKey = JSON.stringify(activeLens);
    const selectionKey = `${selectedNodeId ?? ""}\u0000${selectedNodeKind ?? ""}`;
    if (
      seedsKey !== prevSeedsRef.current ||
      graphMode !== prevModeRef.current ||
      activeWorkspaceId !== prevWorkspaceRef.current ||
      lensKey !== prevLensRef.current ||
      representationMode !== prevRepresentationRef.current ||
      selectionKey !== prevSelectionRef.current
    ) {
      prevSeedsRef.current = seedsKey;
      prevModeRef.current = graphMode;
      prevWorkspaceRef.current = activeWorkspaceId;
      prevLensRef.current = lensKey;
      prevRepresentationRef.current = representationMode;
      prevSelectionRef.current = selectionKey;
      pushState();
    }
  }, [
    activeLens,
    activeWorkspaceId,
    graphMode,
    pushState,
    representationMode,
    seeds,
    selectedNodeId,
    selectedNodeKind,
  ]);

  const canUndo = historyState.index > 0;
  const canRedo = historyState.index < historyState.entries.length - 1;

  return { pushState, undo, redo, canUndo, canRedo };
}
