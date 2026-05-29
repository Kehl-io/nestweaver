import { useCallback, useEffect, useRef } from "react";
import { useStore } from "../stores";
import type { GraphMode } from "../api/types";

interface NavState {
  seeds: string[];
  graphMode: GraphMode;
  selectedNodeId: string | null;
}

const MAX_HISTORY = 50;

export function useNavigationHistory() {
  const setSeeds = useStore((s) => s.setSeeds);
  const setGraphMode = useStore((s) => s.setGraphMode);
  const selectNode = useStore((s) => s.selectNode);

  const historyRef = useRef<NavState[]>([]);
  const indexRef = useRef(-1);
  const isNavigating = useRef(false);

  const pushState = useCallback(() => {
    if (isNavigating.current) return;
    const state = useStore.getState();
    const entry: NavState = {
      seeds: [...state.seeds],
      graphMode: state.graphMode,
      selectedNodeId: state.selectedNodeId,
    };

    // Trim forward history
    historyRef.current = historyRef.current.slice(0, indexRef.current + 1);
    historyRef.current.push(entry);
    if (historyRef.current.length > MAX_HISTORY) {
      historyRef.current.shift();
    } else {
      indexRef.current += 1;
    }
  }, []);

  const undo = useCallback(() => {
    if (indexRef.current <= 0) return;
    isNavigating.current = true;
    indexRef.current -= 1;
    const entry = historyRef.current[indexRef.current];
    setSeeds(entry.seeds);
    setGraphMode(entry.graphMode);
    selectNode(entry.selectedNodeId);
    isNavigating.current = false;
  }, [setSeeds, setGraphMode, selectNode]);

  const redo = useCallback(() => {
    if (indexRef.current >= historyRef.current.length - 1) return;
    isNavigating.current = true;
    indexRef.current += 1;
    const entry = historyRef.current[indexRef.current];
    setSeeds(entry.seeds);
    setGraphMode(entry.graphMode);
    selectNode(entry.selectedNodeId);
    isNavigating.current = false;
  }, [setSeeds, setGraphMode, selectNode]);

  // Auto-push when seeds or graphMode change (unless we're navigating via undo/redo)
  const seeds = useStore((s) => s.seeds);
  const graphMode = useStore((s) => s.graphMode);
  const prevSeedsRef = useRef<string>(JSON.stringify(seeds));
  const prevModeRef = useRef<GraphMode>(graphMode);

  useEffect(() => {
    const seedsKey = JSON.stringify(seeds);
    if (
      seedsKey !== prevSeedsRef.current ||
      graphMode !== prevModeRef.current
    ) {
      prevSeedsRef.current = seedsKey;
      prevModeRef.current = graphMode;
      pushState();
    }
  }, [seeds, graphMode, pushState]);

  const canUndo = indexRef.current > 0;
  const canRedo = indexRef.current < historyRef.current.length - 1;

  return { pushState, undo, redo, canUndo, canRedo };
}
