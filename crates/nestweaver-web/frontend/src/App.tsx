import { useState, useEffect } from "react";
import { Group, Panel, Separator } from "react-resizable-panels";
import { HotkeysProvider, useHotkeys } from "react-hotkeys-hook";
import { TopBar } from "./components/TopBar";
import { StatusBar } from "./components/StatusBar";
import { ExplorerPanel } from "./components/explorer/ExplorerPanel";
import { DetailPanel } from "./components/detail/DetailPanel";
import { GraphPanel } from "./components/graph/GraphPanel";
import { CanvasView } from "./components/canvas/CanvasView";
import { PresentationView } from "./components/presentation/PresentationView";
import { SourceEvidencePanel } from "./components/workspace/SourceEvidencePanel";
import { useKeyboardShortcuts } from "./hooks/useKeyboardShortcuts";
import { useTheme } from "./hooks/useTheme";
import { useDeepLink } from "./hooks/useDeepLink";
import { useWasmEngineLifecycle } from "./hooks/useWasmEngine";
import { ErrorBoundary } from "./components/ErrorBoundary";
import { useStore } from "./stores";
import { ShortcutsOverlay } from "./components/ShortcutsOverlay";
import { LiveAnnouncer } from "./components/shared/LiveAnnouncer";
import { ToastViewport } from "./components/shared/ToastViewport";
import { LlmQueryBar } from "./components/llm/LlmQueryBar";
import { InspectorDrawer } from "./components/InspectorDrawer";

function ResizeHandle() {
  return (
    <Separator className="w-1 cursor-col-resize bg-[var(--color-border)] transition-colors hover:bg-[var(--color-graph-selection)]" />
  );
}

function AppContent() {
  useKeyboardShortcuts();
  useTheme();
  useDeepLink();
  useWasmEngineLifecycle();
  const activeView = useStore((s) => s.activeView);
  const layoutMode = useStore((s) => s.layoutMode);
  const setLayoutMode = useStore((s) => s.setLayoutMode);
  const selectedNodeId = useStore((s) => s.selectedNodeId);
  const modalOpen = useStore((s) => s.llmBarOpen || s.shortcutsOpen);
  // Responsive breakpoint detection
  const [width, setWidth] = useState(window.innerWidth);
  useEffect(() => {
    const handler = () => setWidth(window.innerWidth);
    window.addEventListener("resize", handler);
    return () => window.removeEventListener("resize", handler);
  }, []);

  // Zen mode keyboard shortcuts
  // mod+k is taken by LLM bar; use mod+shift+g to toggle zen mode
  useHotkeys(
    "mod+shift+g",
    (e) => {
      e.preventDefault();
      setLayoutMode(layoutMode === "zen" ? "panels" : "zen");
    },
    { enableOnFormTags: ["INPUT"], enabled: !modalOpen },
  );

  // Escape is handled by GraphPanel's keyboard nav (closes preview, then deselects).
  // Zen ↔ panels toggle is Cmd+Shift+G only.

  // Determine effective layout based on zen mode and viewport width
  const isZen = layoutMode === "zen";
  // Responsive: below 900px behaves like zen (graph only), 900-1199 hides explorer
  const hideExplorer = isZen || width < 1200;
  const hideEvidence = !isZen && width < 980;
  const hideDetail = !isZen && width < 900;
  const graphDefaultSize = !hideExplorer && !hideEvidence && !hideDetail
    ? "42%"
    : !hideEvidence && !hideDetail
      ? "58%"
      : hideExplorer
        ? "78%"
        : "62%";

  const graphContent = activeView === "canvas" ? (
    <CanvasView />
  ) : activeView === "presentation" ? (
    <PresentationView />
  ) : (
    <GraphPanel />
  );
  // nw-565: the skip link's target; Tab from here reaches the graph chrome.
  const graphView = (
    <main id="main-content" tabIndex={-1} aria-label="Graph and results" className="h-full outline-none">
      {graphContent}
    </main>
  );

  return (
    <div className="flex h-full flex-col overflow-hidden">
      <a
        href="#main-content"
        onClick={(event) => {
          event.preventDefault();
          document.getElementById("main-content")?.focus();
        }}
        className="sr-only focus:not-sr-only focus:fixed focus:left-3 focus:top-3 focus:z-[100] focus:rounded focus:border focus:border-[var(--color-graph-selection)] focus:bg-[var(--color-surface)] focus:px-3 focus:py-2 focus:text-sm focus:font-medium focus:text-[var(--color-text)] focus:shadow-lg"
      >
        Skip to graph
      </a>
      <TopBar />
      {isZen ? (
        // Zen mode: graph takes full area, with a compact evidence path for the selected node.
        <div className="flex-1 min-h-0 relative">
          <ErrorBoundary>
            {graphView}
          </ErrorBoundary>
          {selectedNodeId && activeView === "graph" && (
            <div className="absolute bottom-14 right-3 top-14 z-30 hidden w-[min(360px,calc(100vw-1.5rem))] overflow-hidden rounded border border-[var(--color-border)] shadow-xl md:block">
              <ErrorBoundary>
                <SourceEvidencePanel compact />
              </ErrorBoundary>
            </div>
          )}
        </div>
      ) : (
        // Normal / responsive layout
        <div className="relative flex min-h-0 flex-1">
        <Group
          orientation="horizontal"
          className="flex-1 min-h-0"
        >
          {!hideExplorer && (
            <>
              <Panel
                id="explorer"
                defaultSize="18%"
                minSize="180px"
                maxSize="35%"
                collapsible
              >
                <ExplorerPanel />
              </Panel>
              <ResizeHandle />
            </>
          )}
          <Panel id="graph" defaultSize={graphDefaultSize} minSize="30%">
            <ErrorBoundary>
              {graphView}
            </ErrorBoundary>
          </Panel>
          {!hideEvidence && activeView === "graph" && (
            <>
              <ResizeHandle />
              <Panel
                id="evidence"
                defaultSize="20%"
                minSize="220px"
                maxSize="34%"
                collapsible
              >
                <ErrorBoundary>
                  <SourceEvidencePanel />
                </ErrorBoundary>
              </Panel>
            </>
          )}
          {!hideDetail && (
            <>
              <ResizeHandle />
              <Panel
                id="detail"
                defaultSize="20%"
                minSize="180px"
                maxSize="40%"
                collapsible
              >
                <ErrorBoundary>
                  <DetailPanel />
                </ErrorBoundary>
              </Panel>
            </>
          )}
        </Group>
        {/* Always mounted so it can close itself when the window widens. */}
        <InspectorDrawer />
        </div>
      )}
      <StatusBar />
      <LlmQueryBar />
      <ShortcutsOverlay />
      <LiveAnnouncer />
      <ToastViewport />
    </div>
  );
}

export default function App() {
  return (
    <HotkeysProvider>
      <AppContent />
    </HotkeysProvider>
  );
}
