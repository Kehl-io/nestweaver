import { useCallback, useEffect, useRef } from "react";

import { GraphCanvas } from "./GraphCanvas";
import { GraphMatrixView } from "./GraphMatrixView";
import { GraphMinimap } from "./GraphMinimap";
import { NodeListView } from "./NodeListView";
import { ContextMenu } from "./ContextMenu";
import { ModeTabs } from "./ModeTabs";
import { ControlDock } from "./ControlDock";
import { NodePreviewCard } from "./NodePreviewCard";
import { GraphLegend } from "./GraphLegend";
import { PathTargetSelector } from "../PathTargetSelector";
import { DiffSeedInput } from "../DiffSeedInput";
import { OverviewCommandShelf } from "../overview/OverviewCommandShelf";
import { OverviewContextSurface } from "../overview/OverviewContextSurface";
import { JsonResultView } from "../workspace/JsonResultView";
import { LensSummaryPanel } from "../workspace/LensSummaryPanel";
import { WorkspaceToolbar } from "../workspace/WorkspaceToolbar";
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
 * - Ctrl+Tab / Alt+Tab: cycle nodes ordered by PageRank relevance
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
        const { previewNodeId, closePreview: close } = useStore.getState();
        if (previewNodeId) {
          close();
        } else {
          selectNode(null);
        }
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

      if (e.key === "Tab" && (e.ctrlKey || e.altKey)) {
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
  const representationMode = useStore((s) => s.representationMode);
  const layoutMode = useStore((s) => s.layoutMode);
  const selectedNodeId = useStore((s) => s.selectedNodeId);
  const setGraphMode = useStore((s) => s.setGraphMode);
  const semanticLayoutRequested = useStore((s) => s.semanticLayoutRequested);
  const clearSemanticLayoutRequest = useStore(
    (s) => s.clearSemanticLayoutRequest,
  );
  const { applySemanticLayout } = useSemanticLayout();

  useEffect(() => {
    if ((graphMode === "local" || graphMode === "impact") && !selectedNodeId) {
      setGraphMode("overview");
    }
  }, [graphMode, selectedNodeId, setGraphMode]);

  useEffect(() => {
    if (semanticLayoutRequested) {
      applySemanticLayout();
      clearSemanticLayoutRequest();
    }
  }, [semanticLayoutRequested, applySemanticLayout, clearSemanticLayoutRequest]);

  return (
    <>
      {graphMode === "overview" && representationMode === "graph" && layoutMode !== "zen" && (
        <>
          <OverviewCommandShelf {...overviewState} />
          <OverviewContextSurface
            overview={overviewState.overview}
          />
        </>
      )}
      {graphMode === "local" && representationMode === "graph" && (
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
            className="w-24 accent-[var(--color-graph-selection)]"
          />
        </div>
      )}
    </>
  );
}

const ZERO_NODE_IMPACT_RESULTS = new Set([
  "no-match",
  "unsupported",
  "partial",
  "truncated",
  "error",
  "timed-out",
  "cancelled",
  "empty",
]);

function resultLabel(result: string): string {
  return result.replace(/-/g, " ");
}

function impactStatusTone(result: string, hasUnsupported: boolean): string {
  if (result === "error" || result === "timed-out" || result === "cancelled") {
    return "border-red-500/35 bg-red-500/10 text-red-200";
  }
  if (result === "unsupported" || hasUnsupported) {
    return "border-amber-500/35 bg-amber-500/10 text-amber-200";
  }
  if (result === "no-match" || result === "empty") {
    return "border-[var(--color-border)] bg-[var(--color-surface-alt)]/90 text-[var(--color-text)]";
  }
  return "border-sky-500/35 bg-sky-500/10 text-sky-200";
}

function ZeroNodeImpactOverlay() {
  const activeLens = useStore((s) => s.activeLens);
  const sceneMetadata = useStore((s) => s.sceneMetadata);
  const trustSummary = useStore((s) => s.trustSummary);
  const result = trustSummary?.result ?? sceneMetadata?.trust.result ?? "";
  const unsupported =
    trustSummary?.unsupported ?? sceneMetadata?.trust.unsupported ?? [];
  const message =
    trustSummary?.message ??
    sceneMetadata?.trust.message ??
    "Impact result metadata is unavailable.";

  if (!ZERO_NODE_IMPACT_RESULTS.has(result)) return null;

  return (
    <section
      aria-label="Impact result state"
      className={`absolute left-3 top-3 z-30 w-[min(360px,calc(100%-1.5rem))] rounded border p-3 text-xs shadow-lg backdrop-blur ${impactStatusTone(
        result,
        unsupported.length > 0,
      )}`}
    >
      <div className="flex min-w-0 items-center justify-between gap-3">
        <p className="truncate font-semibold text-[var(--color-text)]">
          {activeLens.lens === "impact" ? activeLens.label : "Impact"}
        </p>
        <span className="shrink-0 rounded border border-current/25 px-2 py-0.5 text-[10px] uppercase">
          {resultLabel(result)}
        </span>
      </div>
      <p className="mt-2 leading-5 text-[var(--color-text-muted)]">
        {message}
      </p>
      {unsupported.length > 0 && (
        <p className="mt-2 break-words text-[11px] leading-5 text-amber-300">
          Unavailable: {unsupported.join(", ")}
        </p>
      )}
    </section>
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
  const representationMode = useStore((s) => s.representationMode);
  const graphMode = useStore((s) => s.graphMode);
  const minimapVisible = useStore((s) => s.minimapVisible);
  const layoutMode = useStore((s) => s.layoutMode);
  const graphInstance = useStore((s) => s.graphInstance);
  const sceneMetadata = useStore((s) => s.sceneMetadata);
  const trustSummary = useStore((s) => s.trustSummary);
  const focusMap = layoutMode === "zen";
  const graphRepresentationActive = representationMode === "graph";
  const trustResult = trustSummary?.result ?? sceneMetadata?.trust.result ?? "";
  const zeroNodeImpactActive =
    graphRepresentationActive &&
    graphMode === "impact" &&
    (graphInstance?.order ?? 0) === 0 &&
    ZERO_NODE_IMPACT_RESULTS.has(trustResult);

  const graphPanelRef = useRef<HTMLDivElement>(null);
  useGraphKeyboardNav(graphPanelRef);

  const contextMenu = useStore((s) => s.contextMenu);
  const closeContextMenu = useStore((s) => s.closeContextMenu);

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
      <WorkspaceToolbar />
      <ModeTabs />
      <div className="flex-1 relative bg-[var(--color-surface)]">
        <div
          ref={graphPanelRef}
          aria-label={
            representationMode === "json"
              ? "JSON result view"
              : representationMode === "table" || representationMode === "list"
                ? "Node table view"
                : representationMode === "matrix"
                  ? "Graph matrix view"
                : "Code knowledge graph"
          }
          role={graphRepresentationActive ? "application" : "region"}
          tabIndex={0}
          style={{ background: "var(--color-graph-bg)", width: "100%", height: "100%" }}
          onContextMenu={handleContextMenu}
        >
          {representationMode === "json" ? (
            <JsonResultView />
          ) : viewMode === "list" || representationMode === "table" ? (
            <NodeListView />
          ) : representationMode === "matrix" ? (
            <GraphMatrixView />
          ) : (
            <GraphCanvas />
          )}
        </div>
        {/* Mode hooks run outside the R3F canvas — they only need zustand, not a 3D context */}
        <GraphModeHooks />
        {graphRepresentationActive && <ControlDock />}
        {graphRepresentationActive && <NodePreviewCard />}
        {graphRepresentationActive && !focusMap && <GraphLegend />}
        {graphRepresentationActive && minimapVisible && graphMode !== "overview" && !focusMap && (
          <div className="absolute bottom-14 right-3 z-10 opacity-80 transition-opacity hover:opacity-100">
            <GraphMinimap />
          </div>
        )}
        {zeroNodeImpactActive && <ZeroNodeImpactOverlay />}
        {graphRepresentationActive && graphMode !== "overview" && !focusMap && !zeroNodeImpactActive && (
          <div className="absolute left-3 top-3 z-20 w-[min(320px,calc(100%-1.5rem))]">
            <LensSummaryPanel compact />
          </div>
        )}
        {contextMenu && (
          <ContextMenu
            x={contextMenu.x}
            y={contextMenu.y}
            nodeId={contextMenu.nodeId}
            onClose={closeContextMenu}
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
    </div>
  );
}
