import { useState, useCallback, useEffect } from "react";
import { SigmaContainer } from "@react-sigma/core";
import type { Settings } from "sigma/settings";
import "@react-sigma/core/lib/style.css";

import { GraphEvents } from "./GraphEvents";
import { GraphReducers } from "./GraphReducers";
import { ContextMenu } from "./ContextMenu";
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
import { useInspectorMode } from "./modes/useInspectorMode";
import { useSemanticZoom } from "./useSemanticZoom";
import { useSemanticLayout } from "./modes/useSemanticLayout";
import { useStore } from "../../stores";

function GraphContent() {
  // ALL hooks called unconditionally — they no-op when their mode isn't active
  useContextMode();
  useImpactMode();
  useReposMode();
  useFeaturesMode();
  useInspectorMode();

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

export function GraphPanel() {
  const pathfindingActive = useStore((s) => s.pathfindingActive);
  const pathfindingTo = useStore((s) => s.pathfindingTo);
  const diffActive = useStore((s) => s.diffActive);
  const diffState = useStore((s) => s.diffState);

  return (
    <div className="flex h-full flex-col relative">
      <div className="flex-1 relative bg-[var(--color-surface)]">
        <SigmaContainer
          style={{ height: "100%", width: "100%" }}
          settings={{
            renderEdgeLabels: false,
            enableEdgeEvents: true,
            labelDensity: 0.07,
            labelGridCellSize: 60,
            labelRenderedSizeThreshold: 8,
            labelFont: "system-ui, sans-serif",
            labelSize: 12,
            zIndex: true,
          } satisfies Partial<Settings>}
        >
          <GraphContent />
          <MinimapOverlay />
          <CommunityOverlay />
        </SigmaContainer>
        <GraphToolbar />
        {pathfindingActive && !pathfindingTo && <PathTargetSelector />}
        {diffActive && !diffState.snapshotB && <DiffSeedInput />}
      </div>
      <TimelineSlider />
      <ModeTabs />
      <LlmQueryBar />
    </div>
  );
}
