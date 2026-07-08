import {
  CornerDownLeft,
  LocateFixed,
  Map,
  Redo2,
  Sparkles,
  Undo2,
} from "lucide-react";
import { useNavigationHistory } from "../../hooks/useNavigationHistory";
import { useStore } from "../../stores";
import { RepresentationTabs } from "./RepresentationTabs";
import { SceneBreadcrumbs } from "./SceneBreadcrumbs";

export function WorkspaceToolbar() {
  const { undo, redo, canUndo, canRedo } = useNavigationHistory();
  const minimapVisible = useStore((s) => s.minimapVisible);
  const toggleMinimap = useStore((s) => s.toggleMinimap);
  const selectedNodeId = useStore((s) => s.selectedNodeId);
  const selectedNodeKind = useStore((s) => s.selectedNodeKind);
  const selectNode = useStore((s) => s.selectNode);
  const graphInstance = useStore((s) => s.graphInstance);
  const setGraphData = useStore((s) => s.setGraphData);
  const requestSemanticLayout = useStore((s) => s.requestSemanticLayout);
  const notify = useStore((s) => s.notify);

  function centerSelection() {
    if (!selectedNodeId) {
      notify({
        kind: "info",
        title: "No selected node",
        message: "Select a node before centering the graph on it.",
      });
      return;
    }
    selectNode(selectedNodeId, selectedNodeKind);
    if (graphInstance) setGraphData(graphInstance);
  }

  return (
    <header className="flex min-h-11 shrink-0 items-center gap-2 border-b border-[var(--color-border)] bg-[var(--color-surface)] px-2">
      <div className="min-w-0 flex-1">
        <SceneBreadcrumbs />
      </div>
      <div className="flex shrink-0 items-center gap-1">
        <button
          type="button"
          onClick={undo}
          disabled={!canUndo}
          title="Undo scene navigation"
          aria-label="Undo scene navigation"
          className="inline-flex h-8 w-8 items-center justify-center rounded border border-[var(--color-border)] text-[var(--color-text-muted)] outline-none hover:bg-[var(--color-surface-alt)] hover:text-[var(--color-text)] disabled:cursor-not-allowed disabled:opacity-45 focus-visible:ring-2 focus-visible:ring-[var(--color-graph-selection)]"
        >
          <Undo2 className="h-4 w-4" />
        </button>
        <button
          type="button"
          onClick={redo}
          disabled={!canRedo}
          title="Redo scene navigation"
          aria-label="Redo scene navigation"
          className="inline-flex h-8 w-8 items-center justify-center rounded border border-[var(--color-border)] text-[var(--color-text-muted)] outline-none hover:bg-[var(--color-surface-alt)] hover:text-[var(--color-text)] disabled:cursor-not-allowed disabled:opacity-45 focus-visible:ring-2 focus-visible:ring-[var(--color-graph-selection)]"
        >
          <Redo2 className="h-4 w-4" />
        </button>
        <span className="mx-1 h-6 w-px bg-[var(--color-border)]" />
        <button
          type="button"
          onClick={centerSelection}
          title="Fit to selected node"
          aria-label="Fit to selected node"
          className="inline-flex h-8 w-8 items-center justify-center rounded border border-[var(--color-border)] text-[var(--color-text-muted)] outline-none hover:bg-[var(--color-surface-alt)] hover:text-[var(--color-text)] focus-visible:ring-2 focus-visible:ring-[var(--color-graph-selection)]"
        >
          <LocateFixed className="h-4 w-4" />
        </button>
        <button
          type="button"
          onClick={requestSemanticLayout}
          title="Apply semantic layout"
          aria-label="Apply semantic layout"
          className="inline-flex h-8 w-8 items-center justify-center rounded border border-[var(--color-border)] text-[var(--color-text-muted)] outline-none hover:bg-[var(--color-surface-alt)] hover:text-[var(--color-text)] focus-visible:ring-2 focus-visible:ring-[var(--color-graph-selection)]"
        >
          <Sparkles className="h-4 w-4" />
        </button>
        <button
          type="button"
          onClick={toggleMinimap}
          aria-pressed={minimapVisible}
          title={minimapVisible ? "Hide minimap" : "Show minimap"}
          aria-label={minimapVisible ? "Hide minimap" : "Show minimap"}
          className={`inline-flex h-8 w-8 items-center justify-center rounded border outline-none focus-visible:ring-2 focus-visible:ring-[var(--color-graph-selection)] ${
            minimapVisible
              ? "border-[var(--color-graph-selection)] bg-[var(--color-surface-alt)] text-[var(--color-graph-selection)]"
              : "border-[var(--color-border)] text-[var(--color-text-muted)] hover:bg-[var(--color-surface-alt)] hover:text-[var(--color-text)]"
          }`}
        >
          <Map className="h-4 w-4" />
        </button>
        <span className="hidden items-center gap-1 rounded border border-[var(--color-border)] px-1.5 py-1 text-[10px] text-[var(--color-text-muted)] 2xl:inline-flex">
          <CornerDownLeft className="h-3 w-3" />
          Enter opens
        </span>
        <RepresentationTabs />
      </div>
    </header>
  );
}
