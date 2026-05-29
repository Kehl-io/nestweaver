import { useState, useCallback, useEffect } from "react";

import { GraphCanvas } from "./GraphCanvas";
import { NodeListView } from "./NodeListView";
import { ContextMenu } from "./ContextMenu";
import { NodeFilterBar } from "./NodeFilterBar";
import { ModeTabs } from "./ModeTabs";
import { GraphToolbar } from "./GraphToolbar";
import { PathTargetSelector } from "../PathTargetSelector";
import { DiffSeedInput } from "../DiffSeedInput";
import { LlmQueryBar } from "../llm/LlmQueryBar";
import { TimelineSlider } from "../timeline/TimelineSlider";
import { useContextMode } from "./modes/useContextMode";
import { useImpactMode } from "./modes/useImpactMode";
import { useReposMode } from "./modes/useReposMode";
import { useFeaturesMode } from "./modes/useFeaturesMode";
import { useLocalMode } from "./modes/useLocalMode";
import { useSemanticLayout } from "./modes/useSemanticLayout";
import { useStore } from "../../stores";

/**
 * Runs all mode hooks unconditionally — they no-op when their mode isn't active.
 * These no longer need to be inside a Sigma context; they read/write via zustand.
 */
function GraphModeHooks() {
  useContextMode();
  useImpactMode();
  useReposMode();
  useFeaturesMode();
  const { hops, setHops } = useLocalMode();

  const graphMode = useStore((s) => s.graphMode);
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
          aria-label={viewMode === "list" ? "Node list view" : "Code knowledge graph"}
          role="application"
          tabIndex={0}
          style={{ background: "var(--color-graph-bg)", width: "100%", height: "100%" }}
          onContextMenu={handleContextMenu}
        >
          {viewMode === "list" ? <NodeListView /> : <GraphCanvas />}
        </div>
        {/* Mode hooks run outside the R3F canvas — they only need zustand, not a 3D context */}
        <GraphModeHooks />
        <GraphToolbar />
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
      <NodeFilterBar />
      <ModeTabs />
      <LlmQueryBar />
    </div>
  );
}
