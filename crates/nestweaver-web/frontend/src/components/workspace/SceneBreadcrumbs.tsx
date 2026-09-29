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
  const { data: preview, notFound } = useNodePreview(selectedNodeId, selectedNodeKind);
  const previewName =
    preview?.type === "symbol"
      ? preview.detail.symbol.name
      : preview?.type === "note"
        ? preview.detail.note.title
        : preview?.type === "file"
          ? preview.path.split("/").pop() || preview.path
          : null;

  const graphLabel =
    (selectedNodeId && graphInstance?.hasNode(selectedNodeId)
      ? (graphInstance.getNodeAttribute(selectedNodeId, "label") as
          string | undefined)
      : null) ?? previewName;
  const lensMode = graphModeForLens(activeLens.lens);

  const homeLabel = workspace?.label ?? "All indexed content";
  // nw-663: the nav gets ~190px at 1280px. Priority is selection > home >
  // lens: with a node selected the home crumb collapses to its icon (the
  // name stays in its accessible name and title), the lens crumb shrinks
  // with an ellipsis, and the selection keeps its natural width unless even
  // that does not fit, when it too ends in an ellipsis. The representation
  // crumb repeated RepresentationTabs and is gone.
  const hasSelection = Boolean(selectedNodeId);

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
        className={`inline-flex h-7 items-center gap-1 overflow-hidden rounded px-1.5 font-medium text-[var(--color-text)] outline-none hover:bg-[var(--color-surface-alt)] focus-visible:ring-2 focus-visible:ring-[var(--color-graph-selection)] ${
          hasSelection ? "shrink-0" : "min-w-[4.5rem]"
        }`}
        title={`${homeLabel} (go to overview)`}
        aria-label={hasSelection ? `${homeLabel} (go to overview)` : undefined}
      >
        <Home className="h-3.5 w-3.5 shrink-0" />
        <span className={hasSelection ? "sr-only" : "min-w-0 max-w-[8rem] truncate"}>
          {homeLabel}
        </span>
      </button>
      <ChevronRight className="h-3.5 w-3.5 shrink-0" aria-hidden="true" />
      <button
        type="button"
        onClick={() => {
          if (lensMode) setGraphMode(lensMode);
        }}
        className="h-7 min-w-[2rem] max-w-[8rem] truncate rounded px-1.5 font-medium text-[var(--color-text)] outline-none hover:bg-[var(--color-surface-alt)] focus-visible:ring-2 focus-visible:ring-[var(--color-graph-selection)]"
        title={`Lens: ${activeLens.label}`}
      >
        {activeLens.label}
      </button>
      {hasSelection && (
        <>
          <ChevronRight className="h-3.5 w-3.5 shrink-0" aria-hidden="true" />
          <span
            className="shrink-0 truncate rounded px-1.5 py-1 font-medium text-[var(--color-text)]"
            // Whole when it fits; otherwise capped to the room left after the
            // icon-only home crumb, both chevrons and a minimal lens crumb,
            // so a long name ends in an ellipsis instead of being clipped.
            style={{ maxWidth: "min(10rem, calc(100% - 6.5rem))" }}
            title={notFound ? `Not found: ${selectedNodeId}` : selectedNodeId ?? undefined}
          >
            {notFound ? "Not found" : compactNodeLabel(selectedNodeId, graphLabel)}
          </span>
        </>
      )}
    </nav>
  );
}
