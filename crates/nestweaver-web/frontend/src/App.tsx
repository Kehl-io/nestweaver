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
import { ErrorBoundary } from "./components/ErrorBoundary";
import { useStore } from "./stores";

function ResizeHandle() {
  return (
    <Separator className="w-1 bg-[var(--color-border)] hover:bg-blue-500 transition-colors cursor-col-resize" />
  );
}

function AppContent() {
  useKeyboardShortcuts();
  useTheme();
  const activeView = useStore((s) => s.activeView);
  const layoutMode = useStore((s) => s.layoutMode);
  const setLayoutMode = useStore((s) => s.setLayoutMode);

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
    <div className="h-full flex flex-col">
      <TopBar />
      {isZen ? (
        // Zen mode: graph takes full area, detail floats
        <div className="flex-1 min-h-0 relative">
          <ErrorBoundary>
            {graphView}
          </ErrorBoundary>
          {/* Floating detail panel */}
          <div
            style={{
              position: "fixed",
              bottom: "48px",
              right: "16px",
              width: "300px",
              maxHeight: "60vh",
              overflowY: "auto",
              background: "var(--color-surface)",
              opacity: 0.95,
              borderRadius: "12px",
              border: "1px solid var(--color-border)",
              boxShadow: "0 8px 32px rgba(0,0,0,0.18)",
              zIndex: 50,
            }}
          >
            <ErrorBoundary>
              <DetailPanel />
            </ErrorBoundary>
          </div>
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
                defaultSize="20%"
                minSize="180px"
                maxSize="35%"
                collapsible
              >
                <ExplorerPanel />
              </Panel>
              <ResizeHandle />
            </>
          )}
          <Panel id="graph" defaultSize={hideExplorer ? "75%" : "55%"} minSize="30%">
            <ErrorBoundary>
              {graphView}
            </ErrorBoundary>
          </Panel>
          {!hideDetail && (
            <>
              <ResizeHandle />
              <Panel
                id="detail"
                defaultSize="25%"
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
