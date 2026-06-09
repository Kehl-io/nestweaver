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
import { useKeyboardShortcuts } from "./hooks/useKeyboardShortcuts";
import { useTheme } from "./hooks/useTheme";
import { useDeepLink } from "./hooks/useDeepLink";
import { useWasmEngine } from "./hooks/useWasmEngine";
import { ErrorBoundary } from "./components/ErrorBoundary";
import { useStore } from "./stores";

function ResizeHandle() {
  return (
    <Separator className="w-1 cursor-col-resize bg-[var(--color-border)] transition-colors hover:bg-[var(--color-graph-selection)]" />
  );
}

function AppContent() {
  useKeyboardShortcuts();
  useTheme();
  useDeepLink();
  useWasmEngine();
  const activeView = useStore((s) => s.activeView);
  const layoutMode = useStore((s) => s.layoutMode);
  const setLayoutMode = useStore((s) => s.setLayoutMode);
  const selectedNodeId = useStore((s) => s.selectedNodeId);

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
    { enableOnFormTags: ["INPUT"] },
  );

  useHotkeys(
    "escape",
    () => {
      if (layoutMode === "zen") setLayoutMode("panels");
    },
    { enableOnFormTags: false },
  );

  // Determine effective layout based on zen mode and viewport width
  const isZen = layoutMode === "zen";
  // Responsive: below 900px behaves like zen (graph only), 900-1199 hides explorer
  const hideExplorer = isZen || width < 1200;
  const hideDetail = !isZen && width < 900;

  const graphView = activeView === "canvas" ? (
    <CanvasView />
  ) : activeView === "presentation" ? (
    <PresentationView />
  ) : (
    <GraphPanel />
  );

  return (
    <div className="flex h-full flex-col overflow-hidden">
      <TopBar />
      {isZen ? (
        // Zen mode: graph takes full area, detail floats
        <div className="flex-1 min-h-0 relative">
          <ErrorBoundary>
            {graphView}
          </ErrorBoundary>
          {selectedNodeId && (
            <div
              style={{
                position: "fixed",
                bottom: "24px",
                right: "16px",
                width: "320px",
                maxHeight: "58vh",
                overflowY: "auto",
                background: "var(--color-surface)",
                opacity: 0.96,
                borderRadius: "8px",
                border: "1px solid var(--color-border)",
                boxShadow: "0 10px 30px rgba(15,23,42,0.14)",
                zIndex: 50,
              }}
            >
              <ErrorBoundary>
                <DetailPanel />
              </ErrorBoundary>
            </div>
          )}
        </div>
      ) : (
        // Normal / responsive layout
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
          <Panel id="graph" defaultSize={hideExplorer ? "78%" : "62%"} minSize="36%">
            <ErrorBoundary>
              {graphView}
            </ErrorBoundary>
          </Panel>
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
      )}
      <StatusBar />
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
