import { Group, Panel, Separator } from "react-resizable-panels";
import { HotkeysProvider } from "react-hotkeys-hook";
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

export default function App() {
  useKeyboardShortcuts();
  useTheme();
  const activeView = useStore((s) => s.activeView);

  return (
    <HotkeysProvider>
      <div className="h-full flex flex-col">
        <TopBar />
        <Group
          orientation="horizontal"
          className="flex-1 min-h-0"
        >
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
          <Panel id="graph" defaultSize="55%" minSize="30%">
            <ErrorBoundary>
              {activeView === "canvas" ? (
                <CanvasView />
              ) : activeView === "presentation" ? (
                <PresentationView />
              ) : (
                <GraphPanel />
              )}
            </ErrorBoundary>
          </Panel>
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
        </Group>
        <StatusBar />
      </div>
    </HotkeysProvider>
  );
}
