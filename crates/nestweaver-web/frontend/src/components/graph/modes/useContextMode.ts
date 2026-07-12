import { useCallback, useEffect, useRef } from "react";
import type Graph from "graphology";
import { useStore } from "../../../stores";
import { useForceLayout } from "../../../hooks/useForceLayout";
import { api } from "../../../api/client";
import type { BrainContextResult } from "../../../api/types";
import type { SceneMetadata } from "../../../api/p1Types";
import {
  workspaceContextBody,
  workspaceSceneMetadataWithResult,
} from "../../../api/workspaces";
import { buildGraphFromContext, finalizeNodeSizes } from "../utils/buildGraphFromContext";
import { preserveGraphLayout } from "../utils/preserveGraphLayout";

const MAX_LAYOUT_MS = 10_000;
type ScopedBrainContextResult = BrainContextResult & { _meta?: SceneMetadata };

function sameSeeds(left: string[], right: string[]): boolean {
  if (left.length !== right.length) return false;
  return left.every((seed, index) => seed === right[index]);
}

function contextLayoutKey(seeds: string[]): string {
  return JSON.stringify(seeds);
}

function loadErrorMessage(err: unknown, fallback: string): string {
  return err instanceof Error && err.message ? err.message : fallback;
}

async function loadScopedBrainContext(
  seeds: string[],
  tokenBudget: number,
  workspaceId: string,
): Promise<ScopedBrainContextResult> {
  const response = await fetch("/api/v1/brain/context", {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(workspaceContextBody(seeds, tokenBudget, workspaceId)),
  });
  if (!response.ok) {
    const body = await response.json().catch(() => ({ error: response.statusText }));
    throw new Error(body.error || response.statusText);
  }
  return response.json() as Promise<ScopedBrainContextResult>;
}

export function useContextMode() {
  const setGraphData = useStore((s) => s.setGraphData);
  const clearGraphData = useStore((s) => s.clearGraphData);
  const notify = useStore((s) => s.notify);
  const seeds = useStore((s) => s.seeds);
  const graphMode = useStore((s) => s.graphMode);
  const activeWorkspaceId = useStore((s) => s.activeWorkspaceId);
  const setActiveLens = useStore((s) => s.setActiveLens);
  const setSceneMetadata = useStore((s) => s.setSceneMetadata);
  const stopTimerRef = useRef<ReturnType<typeof setTimeout> | null>(null);
  const requestIdRef = useRef(0);
  const previousLayoutRef = useRef<{ key: string; graph: Graph } | null>(null);

  const { start, stop, kill, isRunning } = useForceLayout();

  const loadContextData = useCallback(async () => {
    if (graphMode !== "context") {
      requestIdRef.current += 1;
      return;
    }

    setActiveLens({ lens: "context", label: "Context", targetUid: seeds[0] ?? null, workspaceId: activeWorkspaceId || "all" });

    if (seeds.length === 0) {
      requestIdRef.current += 1;
      return;
    }

    const requestId = ++requestIdRef.current;
    const requestSeeds = [...seeds];
    const requestWorkspaceId = activeWorkspaceId || "all";
    const layoutKey = `${requestWorkspaceId}:${contextLayoutKey(requestSeeds)}`;
    const isCurrentRequest = () => {
      const state = useStore.getState();
      return (
        requestId === requestIdRef.current &&
        state.graphMode === "context" &&
        state.activeWorkspaceId === requestWorkspaceId &&
        sameSeeds(state.seeds, requestSeeds)
      );
    };

    try {
      const currentState = useStore.getState();
      const previousWorkspaceId =
        currentState.sceneMetadata?.workspace_id ??
        currentState.activeLens.workspaceId ??
        "all";
      if (previousWorkspaceId !== requestWorkspaceId) {
        clearGraphData();
        setSceneMetadata(
          workspaceSceneMetadataWithResult(
            currentState.selectedWorkspace()?._meta,
            "loading",
            `Loading ${currentState.selectedWorkspace()?.label ?? requestWorkspaceId}.`,
          ),
        );
      }
      const result = await loadScopedBrainContext(
        requestSeeds,
        2000,
        requestWorkspaceId,
      );
      if (!isCurrentRequest()) return;

      const graph = buildGraphFromContext(result);

      for (const seed of result.seeds) {
        try {
          const detail = await api.symbol(seed.uid);
          if (!isCurrentRequest()) return;

          for (const caller of detail.callers) {
            if (
              graph.hasNode(caller.uid) &&
              !graph.hasEdge(caller.uid, seed.uid)
            ) {
              graph.addEdge(caller.uid, seed.uid, {
                type: "arrow",
                size: 1.5,
                color: "#9CA3AF",
                label: "calls",
              });
            }
          }
          for (const callee of detail.callees) {
            if (
              graph.hasNode(callee.uid) &&
              !graph.hasEdge(seed.uid, callee.uid)
            ) {
              graph.addEdge(seed.uid, callee.uid, {
                type: "arrow",
                size: 1.5,
                color: "#9CA3AF",
                label: "calls",
              });
            }
          }
        } catch {
          if (!isCurrentRequest()) return;
          // Symbol lookup fails for notes/tags — skip
        }
      }

      finalizeNodeSizes(graph);
      const previousLayout = previousLayoutRef.current;
      preserveGraphLayout(
        graph,
        previousLayout?.key === layoutKey ? previousLayout.graph : null,
      );
      if (!isCurrentRequest()) return;

      setGraphData(graph);
      setActiveLens({
        lens: "context",
        label: "Context",
        targetUid: requestSeeds[0] ?? null,
        workspaceId: requestWorkspaceId,
      });
      setSceneMetadata(result._meta ?? null);
      previousLayoutRef.current = { key: layoutKey, graph };
      start(graph);
      // Stop after MAX_LAYOUT_MS as a safety ceiling
      if (stopTimerRef.current) clearTimeout(stopTimerRef.current);
      stopTimerRef.current = setTimeout(() => stop(), MAX_LAYOUT_MS);
    } catch (err) {
      if (!isCurrentRequest()) return;

      console.error("Failed to load context:", err);
      const message = loadErrorMessage(err, "Failed to load context graph");
      clearGraphData();
      setSceneMetadata(
        workspaceSceneMetadataWithResult(
          useStore.getState().selectedWorkspace()?._meta,
          "error",
          message,
        ),
      );
      notify({
        kind: "error",
        title: "Context graph failed",
        message,
      });
    }
  }, [
    activeWorkspaceId,
    clearGraphData,
    graphMode,
    notify,
    seeds,
    setActiveLens,
    setGraphData,
    setSceneMetadata,
    start,
    stop,
  ]);

  // Stop the layout automatically when isRunning goes false (convergence detected)
  useEffect(() => {
    if (!isRunning) {
      if (stopTimerRef.current) {
        clearTimeout(stopTimerRef.current);
        stopTimerRef.current = null;
      }
    }
  }, [isRunning]);

  useEffect(() => {
    loadContextData();
    return () => {
      requestIdRef.current += 1;
      if (stopTimerRef.current) clearTimeout(stopTimerRef.current);
      kill();
    };
  }, [loadContextData, kill]);

  return { isRunning, start, stop };
}
