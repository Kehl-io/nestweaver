import { useState, useCallback, useEffect, useRef } from "react";

import { GraphCanvas } from "./GraphCanvas";
import { GraphMatrixView } from "./GraphMatrixView";
import { GraphMinimap } from "./GraphMinimap";
import { NodeListView } from "./NodeListView";
import { ContextMenu } from "./ContextMenu";
import { ModeTabs } from "./ModeTabs";
import { ControlDock } from "./ControlDock";
import { ActiveFilterSummary } from "./ActiveFilterSummary";
import { GraphLegend } from "./GraphLegend";
import { PathTargetSelector } from "../PathTargetSelector";
import { DiffSeedInput } from "../DiffSeedInput";
import { LlmQueryBar } from "../llm/LlmQueryBar";
import { OverviewCommandShelf } from "../overview/OverviewCommandShelf";
import { OverviewContextSurface } from "../overview/OverviewContextSurface";
import { TimelineSlider } from "../timeline/TimelineSlider";
import { useOverviewMode } from "./modes/useOverviewMode";
import { useContextMode } from "./modes/useContextMode";
import { useImpactMode } from "./modes/useImpactMode";
import { useReposMode } from "./modes/useReposMode";
import { useFeaturesMode } from "./modes/useFeaturesMode";
import { useLocalMode } from "./modes/useLocalMode";
import { useSemanticLayout } from "./modes/useSemanticLayout";
import { useStore } from "../../stores";

/**
 * Attaches keyboard navigation to the graph panel div.
 *
 * - Tab / Shift+Tab: cycle nodes ordered by PageRank relevance
 * - Arrow keys: navigate to the connected neighbor closest in that direction
 * - Enter: set selected node as seed
 * - Escape: deselect
 */
function useGraphKeyboardNav(
  panelRef: React.RefObject<HTMLDivElement | null>,
) {
  const graphInstance = useStore((s) => s.graphInstance);
  const graphVersion = useStore((s) => s.graphVersion);
  const selectNode = useStore((s) => s.selectNode);
  const exploreNode = useStore((s) => s.exploreNode);

  // Keep a ref to a pagerank-sorted node list so the keydown handler is stable
  const sortedNodesRef = useRef<string[]>([]);

  useEffect(() => {
    if (!graphInstance) {
      sortedNodesRef.current = [];
      return;
    }
    // Sort nodes by their pagerank attribute descending (higher = more relevant)
    const nodes = graphInstance.nodes();
    nodes.sort((a, b) => {
      const prA =
        typeof graphInstance.getNodeAttribute(a, "pagerank") === "number"
          ? (graphInstance.getNodeAttribute(a, "pagerank") as number)
          : 0;
      const prB =
        typeof graphInstance.getNodeAttribute(b, "pagerank") === "number"
          ? (graphInstance.getNodeAttribute(b, "pagerank") as number)
          : 0;
      return prB - prA;
    });
    sortedNodesRef.current = nodes;
    // graphVersion is read to trigger a rebuild when the graph changes
  }, [graphInstance, graphVersion]);

  useEffect(() => {
    const el = panelRef.current;
    if (!el) return;

    const handleKeyDown = (e: KeyboardEvent) => {
      const graph = useStore.getState().graphInstance;
      const currentId = useStore.getState().selectedNodeId;
      const sorted = sortedNodesRef.current;

      if (e.key === "Escape") {
        e.preventDefault();
        selectNode(null);
        return;
      }

      if (e.key === "Enter") {
        if (currentId) {
          e.preventDefault();
          const kind =
            graph?.hasNode(currentId)
              ? (graph.getNodeAttribute(currentId, "kind") as string | null)
              : null;
          exploreNode(currentId, kind);
        }
        return;
      }

      if (e.key === "Tab") {
        e.preventDefault();
        if (sorted.length === 0) return;
        const idx = currentId ? sorted.indexOf(currentId) : -1;
        const next = e.shiftKey
          ? (idx <= 0 ? sorted.length - 1 : idx - 1)
          : (idx >= sorted.length - 1 ? 0 : idx + 1);
        const uid = sorted[next];
        const kind =
          graph?.hasNode(uid)
            ? (graph.getNodeAttribute(uid, "kind") as string | null)
            : null;
        selectNode(uid, kind);
        return;
      }

      if (
        e.key === "ArrowUp" ||
        e.key === "ArrowDown" ||
        e.key === "ArrowLeft" ||
        e.key === "ArrowRight"
      ) {
        if (!currentId || !graph) return;
        e.preventDefault();

        // Gather all neighbors (both directions)
        const neighbors = graph.neighbors(currentId);
        if (neighbors.length === 0) return;

        const cx =
          typeof graph.getNodeAttribute(currentId, "x") === "number"
            ? (graph.getNodeAttribute(currentId, "x") as number)
            : 0;
        const cy =
          typeof graph.getNodeAttribute(currentId, "y") === "number"
            ? (graph.getNodeAttribute(currentId, "y") as number)
            : 0;

        // Direction vector for the pressed arrow key
        const dirX =
          e.key === "ArrowLeft" ? -1 : e.key === "ArrowRight" ? 1 : 0;
        const dirY =
          e.key === "ArrowUp" ? 1 : e.key === "ArrowDown" ? -1 : 0;

        // Pick neighbor whose angle matches the pressed direction most closely
        let bestUid: string | null = null;
        let bestDot = -Infinity;

        for (const uid of neighbors) {
          const nx =
            typeof graph.getNodeAttribute(uid, "x") === "number"
              ? (graph.getNodeAttribute(uid, "x") as number)
              : 0;
          const ny =
            typeof graph.getNodeAttribute(uid, "y") === "number"
              ? (graph.getNodeAttribute(uid, "y") as number)
              : 0;
          const dx = nx - cx;
          const dy = ny - cy;
          const len = Math.sqrt(dx * dx + dy * dy);
          if (len === 0) continue;
          const dot = (dx / len) * dirX + (dy / len) * dirY;
          if (dot > bestDot) {
            bestDot = dot;
            bestUid = uid;
          }
        }

        if (bestUid) {
          const kind =
            graph.hasNode(bestUid)
              ? (graph.getNodeAttribute(bestUid, "kind") as string | null)
              : null;
          selectNode(bestUid, kind);
        }
      }
    };

    el.addEventListener("keydown", handleKeyDown);
    return () => el.removeEventListener("keydown", handleKeyDown);
  }, [panelRef, selectNode, exploreNode]);
}

/**
 * Runs all mode hooks unconditionally — they no-op when their mode isn't active.
 * Hooks read and write graph state via zustand.
 */
function GraphModeHooks() {
  const overviewState = useOverviewMode();
  useContextMode();
  useImpactMode();
  useReposMode();
  useFeaturesMode();
  const { hops, setHops } = useLocalMode();

  const graphMode = useStore((s) => s.graphMode);
  const viewMode = useStore((s) => s.viewMode);
  const semanticLayoutRequested = useStore((s) => s.semanticLayoutRequested);
  const clearSemanticLayoutRequest = useStore(
    (s) => s.clearSemanticLayoutRequest,
  );
  const { applySemanticLayout } = useSemanticLayout();

  useEffect(() => {
    if (semanticLayoutRequested) {
      applySemanticLayout();
      clearSemanticLayoutRequest();
    }
  }, [semanticLayoutRequested, applySemanticLayout, clearSemanticLayoutRequest]);

  return (
    <>
      {graphMode === "overview" && viewMode === "graph" && (
        <>
          <OverviewCommandShelf {...overviewState} />
          <OverviewContextSurface
            overview={overviewState.overview}
            reload={overviewState.reload}
          />
        </>
      )}
      {graphMode === "local" && (
        <div className="absolute bottom-12 left-1/2 -translate-x-1/2 z-20 flex items-center gap-2 px-3 py-1.5 rounded bg-[var(--color-surface-alt)] shadow text-xs text-[var(--color-text)]">
          <label htmlFor="local-hops-slider" className="whitespace-nowrap">
            Depth: {hops}
          </label>
          <input
            id="local-hops-slider"
            type="range"
            min={1}
            max={4}
            value={hops}
            onChange={(e) => setHops(Number(e.target.value))}
            className="w-24 accent-blue-500"
          />
        </div>
      )}
    </>
  );
}

export function GraphPanel() {
  const pathfindingActive = useStore((s) => s.pathfindingActive);
  const pathfindingTo = useStore((s) => s.pathfindingTo);
  const diffActive = useStore((s) => s.diffActive);
  const diffState = useStore((s) => s.diffState);
  const selectedNodeId = useStore((s) => s.selectedNodeId);
  const selectedNodeKind = useStore((s) => s.selectedNodeKind);
  const viewMode = useStore((s) => s.viewMode);
  const minimapVisible = useStore((s) => s.minimapVisible);

  const graphPanelRef = useRef<HTMLDivElement>(null);
  useGraphKeyboardNav(graphPanelRef);

  const [contextMenu, setContextMenu] = useState<{
    x: number;
    y: number;
    nodeId: string;
  } | null>(null);

  const handleContextMenu = useCallback(
    (e: React.MouseEvent) => {
      e.preventDefault();
    },
    [],
  );

  // Derive a human-readable name from the UID (e.g. "fn:greet" -> "greet")
  const selectedNodeName = selectedNodeId
    ? (selectedNodeId.split(":").pop() ?? selectedNodeId)
    : null;

  return (
    <div data-testid="graph-panel" className="flex h-full flex-col relative">
      <div className="flex-1 relative bg-[var(--color-surface)]">
        <div
          ref={graphPanelRef}
          aria-label={viewMode === "list" ? "Node list view" : "Code knowledge graph"}
          role="application"
          tabIndex={0}
          style={{ background: "var(--color-graph-bg)", width: "100%", height: "100%" }}
          onContextMenu={handleContextMenu}
        >
          {viewMode === "list" ? (
            <NodeListView />
          ) : viewMode === "matrix" ? (
            <GraphMatrixView />
          ) : (
            <GraphCanvas />
          )}
        </div>
        {/* Mode hooks run outside the R3F canvas — they only need zustand, not a 3D context */}
        <GraphModeHooks />
        <ControlDock />
        <GraphLegend />
        {viewMode === "graph" && minimapVisible && (
          <div className="absolute top-2 right-12 z-10">
            <GraphMinimap />
          </div>
        )}
        {contextMenu && (
          <ContextMenu
            x={contextMenu.x}
            y={contextMenu.y}
            nodeId={contextMenu.nodeId}
            onClose={() => setContextMenu(null)}
          />
        )}
        {pathfindingActive && !pathfindingTo && <PathTargetSelector />}
        {diffActive && !diffState.snapshotB && <DiffSeedInput />}
        {/* Screen reader live region for node selection */}
        <div className="sr-only" aria-live="polite" aria-atomic="true">
          {selectedNodeId && selectedNodeName && (
            <>Selected: {selectedNodeName}{selectedNodeKind ? `, ${selectedNodeKind}` : ""}</>
          )}
        </div>
      </div>
      <TimelineSlider />
      <ActiveFilterSummary />
      <ModeTabs />
      <LlmQueryBar />
    </div>
  );
}
