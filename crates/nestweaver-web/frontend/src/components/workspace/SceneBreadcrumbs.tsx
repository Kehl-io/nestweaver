import { ChevronRight, Home } from "lucide-react";
import type { ActiveLens } from "../../api/p1Types";
import type { GraphMode } from "../../api/types";
import { useNodePreview } from "../../hooks/useNodePreview";
import { useStore } from "../../stores";

function graphModeForLens(lens: ActiveLens): GraphMode | null {
  if (lens === "overview" || lens === "context" || lens === "impact")
    return lens;
  return null;
}

function compactNodeLabel(
  uid: string | null,
  graphLabel?: string | null,
): string {
  if (graphLabel) return graphLabel;
  if (!uid) return "No selection";
  return uid.split(":").pop() || uid;
}

export function SceneBreadcrumbs() {
  const workspace = useStore((s) => s.selectedWorkspace());
  const activeLens = useStore((s) => s.activeLens);
  const representationMode = useStore((s) => s.representationMode);
  const selectedNodeId = useStore((s) => s.selectedNodeId);
  const selectedNodeKind = useStore((s) => s.selectedNodeKind);
  const graphInstance = useStore((s) => s.graphInstance);
  const setGraphMode = useStore((s) => s.setGraphMode);
  const setActiveLens = useStore((s) => s.setActiveLens);
  const setRepresentationMode = useStore((s) => s.setRepresentationMode);

  // Prefer the graph node's own label; when the selected node isn't in the
  // current scene (e.g. a symbol selected while in overview/impact, which only
  // show repo/service landmarks) fall back to the resolved detail name so the
  // crumb shows the symbol/note name rather than the uid's numeric line-tail.
  // useNodePreview shares a module-level cache with the detail panel, so this
  // does not add a fetch when the panel is already showing the same node.
  const { data: preview } = useNodePreview(selectedNodeId, selectedNodeKind);
  const previewName =
    preview?.type === "symbol"
      ? preview.detail.symbol.name
      : preview?.type === "note"
        ? preview.detail.note.title
        : null;

  const graphLabel =
    (selectedNodeId && graphInstance?.hasNode(selectedNodeId)
      ? (graphInstance.getNodeAttribute(selectedNodeId, "label") as
          string | undefined)
      : null) ?? previewName;
  const lensMode = graphModeForLens(activeLens.lens);

  return (
    <nav
      aria-label="Scene breadcrumbs"
      className="flex min-w-0 items-center gap-1 overflow-hidden text-[11px] text-[var(--color-text-muted)]"
    >
      <button
        type="button"
        onClick={() => {
          setGraphMode("overview");
          setActiveLens({
            lens: "overview",
            label: "Overview",
            targetUid: null,
            workspaceId: workspace?.id ?? "all",
          });
          setRepresentationMode("graph");
        }}
        className="inline-flex h-7 min-w-0 items-center gap-1 rounded px-1.5 font-medium text-[var(--color-text)] outline-none hover:bg-[var(--color-surface-alt)] focus-visible:ring-2 focus-visible:ring-[var(--color-graph-selection)]"
        title="Go to workspace overview"
      >
        <Home className="h-3.5 w-3.5 shrink-0" />
        <span className="min-w-[3rem] max-w-[8rem] truncate">
          {workspace?.label ?? "All indexed content"}
        </span>
      </button>
      <ChevronRight className="h-3.5 w-3.5 shrink-0" />
      <button
        type="button"
        onClick={() => {
          if (lensMode) setGraphMode(lensMode);
        }}
        className="h-7 min-w-[3rem] max-w-[8rem] shrink-0 truncate rounded px-1.5 font-medium text-[var(--color-text)] outline-none hover:bg-[var(--color-surface-alt)] focus-visible:ring-2 focus-visible:ring-[var(--color-graph-selection)]"
        title={`Lens: ${activeLens.label}`}
      >
        {activeLens.label}
      </button>
      <ChevronRight className="h-3.5 w-3.5 shrink-0" />
      <span
        className="min-w-[4rem] max-w-[10rem] truncate rounded px-1.5 py-1 font-medium text-[var(--color-text)]"
        title={selectedNodeId ?? "No selected node"}
      >
        {compactNodeLabel(selectedNodeId, graphLabel)}
      </span>
      <ChevronRight className="hidden h-3.5 w-3.5 shrink-0 xl:block" />
      <button
        type="button"
        onClick={() => setRepresentationMode(representationMode)}
        className="hidden h-7 shrink-0 rounded px-1.5 font-medium capitalize text-[var(--color-graph-selection)] outline-none hover:bg-[var(--color-surface-alt)] focus-visible:ring-2 focus-visible:ring-[var(--color-graph-selection)] xl:inline"
        title="Current representation"
      >
        {representationMode === "table" ? "table" : representationMode}
      </button>
    </nav>
  );
}
