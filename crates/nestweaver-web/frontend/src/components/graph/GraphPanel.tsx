import { useState, useCallback, useEffect } from "react";
import { SigmaContainer } from "@react-sigma/core";
import type { Settings } from "sigma/settings";
import "@react-sigma/core/lib/style.css";

import { GraphEvents } from "./GraphEvents";
import { GraphReducers } from "./GraphReducers";
import { ContextMenu } from "./ContextMenu";
import { NodeFilterBar } from "./NodeFilterBar";
import { ModeTabs } from "./ModeTabs";
import { GraphToolbar } from "./GraphToolbar";
import { GraphMinimap } from "./GraphMinimap";
import { CommunityOverlay } from "./overlays/CommunityOverlay";
import { PathTargetSelector } from "../PathTargetSelector";
import { DiffSeedInput } from "../DiffSeedInput";
import { LlmQueryBar } from "../llm/LlmQueryBar";
import { TimelineSlider } from "../timeline/TimelineSlider";
import { useContextMode } from "./modes/useContextMode";
import { useImpactMode } from "./modes/useImpactMode";
import { useReposMode } from "./modes/useReposMode";
import { useFeaturesMode } from "./modes/useFeaturesMode";
import { useLocalMode } from "./modes/useLocalMode";
import { useSemanticZoom } from "./useSemanticZoom";
import { useSemanticLayout } from "./modes/useSemanticLayout";
import { useStore } from "../../stores";

function GraphContent() {
  // ALL hooks called unconditionally — they no-op when their mode isn't active
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

  const [contextMenu, setContextMenu] = useState<{
    x: number;
    y: number;
    nodeId: string;
  } | null>(null);

  const handleContextMenu = useCallback(
    (menu: { x: number; y: number; nodeId: string } | null) =>
      setContextMenu(menu),
    [],
  );

  return (
    <>
      <GraphEvents onContextMenu={handleContextMenu} />
      <GraphReducers />
      {contextMenu && (
        <ContextMenu
          x={contextMenu.x}
          y={contextMenu.y}
          nodeId={contextMenu.nodeId}
          onClose={() => setContextMenu(null)}
        />
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

function MinimapOverlay() {
  const minimapVisible = useStore((s) => s.minimapVisible);
  const tier = useSemanticZoom();

  return (
    <>
      {minimapVisible && (
        <div className="absolute top-2 right-12 z-10">
          <GraphMinimap />
        </div>
      )}
      <div className="absolute bottom-2 left-2 z-10 px-2 py-1 text-[10px] font-mono text-[var(--color-text-muted)] bg-[var(--color-surface-alt)] rounded">
        {tier === "packages"
          ? "Packages"
          : tier === "files"
            ? "Files"
            : "Symbols"}
      </div>
    </>
  );
}

function useIsDark(): boolean {
  const theme = useStore((s) => s.theme);
  const [isDark, setIsDark] = useState(() => {
    if (theme === "dark") return true;
    if (theme === "light") return false;
    return window.matchMedia("(prefers-color-scheme: dark)").matches;
  });

  useEffect(() => {
    if (theme === "dark") { setIsDark(true); return; }
    if (theme === "light") { setIsDark(false); return; }
    const mq = window.matchMedia("(prefers-color-scheme: dark)");
    setIsDark(mq.matches);
    const handler = (e: MediaQueryListEvent) => setIsDark(e.matches);
    mq.addEventListener("change", handler);
    return () => mq.removeEventListener("change", handler);
  }, [theme]);

  return isDark;
}

export function GraphPanel() {
  const pathfindingActive = useStore((s) => s.pathfindingActive);
  const pathfindingTo = useStore((s) => s.pathfindingTo);
  const diffActive = useStore((s) => s.diffActive);
  const diffState = useStore((s) => s.diffState);
  const isDark = useIsDark();
  const selectedNodeId = useStore((s) => s.selectedNodeId);
  const selectedNodeKind = useStore((s) => s.selectedNodeKind);

  const labelColor = isDark ? "#94a3b8" : "#374151";

  // Derive a human-readable name from the UID (e.g. "fn:greet" -> "greet")
  const selectedNodeName = selectedNodeId
    ? (selectedNodeId.split(":").pop() ?? selectedNodeId)
    : null;

  return (
    <div data-testid="graph-panel" className="flex h-full flex-col relative">
      <div className="flex-1 relative bg-[var(--color-surface)]">
        <div
          aria-label="Code knowledge graph"
          role="application"
          tabIndex={0}
          style={{ background: "var(--color-graph-bg)", width: "100%", height: "100%" }}
          onContextMenu={(e) => e.preventDefault()}
        >
        <SigmaContainer
          key={isDark ? "dark" : "light"}
          style={{ height: "100%", width: "100%" }}
          settings={{
            renderEdgeLabels: false,
            enableEdgeEvents: true,
            labelDensity: 0.07,
            labelGridCellSize: 60,
            labelRenderedSizeThreshold: 8,
            labelFont: "system-ui, sans-serif",
            labelSize: 12,
            labelColor: { color: labelColor },
            zIndex: true,
          } satisfies Partial<Settings>}
        >
          <GraphContent />
          <MinimapOverlay />
          <CommunityOverlay />
        </SigmaContainer>
        </div>
        <GraphToolbar />
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
