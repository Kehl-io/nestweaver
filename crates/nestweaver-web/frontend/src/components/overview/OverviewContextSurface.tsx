import type { OverviewLandmark, OverviewResponse } from "../../api/types";
import type { SceneMetadata } from "../../api/p1Types";
import { X } from "lucide-react";
import { useStore } from "../../stores";
import { NodeActionBar } from "../actions/NodeActionBar";
import { WorkspaceScopeSummary } from "../workspace/WorkspaceScopeSummary";

interface OverviewContextSurfaceProps {
  overview: (OverviewResponse & { _meta?: SceneMetadata }) | null;
}

function findOverviewItem(
  overview: (OverviewResponse & { _meta?: SceneMetadata }) | null,
  uid: string | null,
): OverviewLandmark | null {
  if (!overview || !uid) return null;
  return (
    overview.start_here.find((item) => item.uid === uid) ??
    overview.landmarks.find((item) => item.uid === uid) ??
    null
  );
}

function compactLocation(location: string): string {
  const parts = location.split("/");
  return parts.length > 3 ? parts.slice(-3).join("/") : location;
}

export function OverviewContextSurface({ overview }: OverviewContextSurfaceProps) {
  const selectedNodeId = useStore((s) => s.selectedNodeId);
  const selectedNodeKind = useStore((s) => s.selectedNodeKind);
  const graphInstance = useStore((s) => s.graphInstance);
  const selectNode = useStore((s) => s.selectNode);

  const overviewItem = findOverviewItem(overview, selectedNodeId);
  const graphSelected =
    selectedNodeId && graphInstance?.hasNode(selectedNodeId)
      ? {
          uid: selectedNodeId,
          label:
            (graphInstance.getNodeAttribute(selectedNodeId, "label") as string | undefined) ??
            selectedNodeId,
          kind:
            (graphInstance.getNodeAttribute(selectedNodeId, "kind") as string | undefined) ??
            selectedNodeKind ??
            "node",
          reason: graphInstance.getNodeAttribute(selectedNodeId, "reason") as
            | string
            | undefined,
          location: graphInstance.getNodeAttribute(selectedNodeId, "location") as
            | string
            | undefined,
        }
      : null;
  const selected = overviewItem ?? graphSelected;

  if (!selected) return null;

  return (
    <aside
      aria-label="Overview context"
      className="absolute bottom-3 right-3 z-30 max-h-[min(320px,calc(100%-1.5rem))] w-[min(320px,calc(100vw-1.5rem))] overflow-hidden rounded-md border border-[var(--color-border)] bg-[var(--color-surface)]/94 shadow-lg backdrop-blur-xl sm:bottom-4 sm:right-4"
    >
      <div className="p-3">
          <div className="flex min-w-0 items-start justify-between gap-3">
            <div className="min-w-0">
              <p className="text-[10px] font-medium uppercase text-[var(--color-text-muted)]">
                {selected.kind}
              </p>
              <h2 className="mt-0.5 truncate text-sm font-semibold text-[var(--color-text)]">
                {selected.label}
              </h2>
            </div>
            <div className="flex shrink-0 items-center gap-1">
            {overviewItem && (
              <span className="shrink-0 rounded bg-[var(--color-surface-alt)] px-1.5 py-0.5 text-[10px] font-medium text-[var(--color-graph-selection)]">
                Overview
              </span>
            )}
            <button
              type="button"
              onClick={() => selectNode(null)}
              aria-label="Back to Start Here"
              title="Clear the selection and return to Start Here"
              className="shrink-0 rounded p-1 text-[var(--color-text-muted)] hover:bg-[var(--color-surface-alt)] hover:text-[var(--color-text)] focus-visible:outline focus-visible:outline-2 focus-visible:outline-[var(--color-graph-selection)]"
            >
              <X className="h-3.5 w-3.5" aria-hidden="true" />
            </button>
            </div>
          </div>

          <p className="mt-2 max-h-12 overflow-hidden text-xs leading-5 text-[var(--color-text-muted)]">
            {selected.reason ?? "Selected overview landmark"}
          </p>

          <div className="mt-3">
            <WorkspaceScopeSummary metadata={overview?._meta} />
          </div>

          <NodeActionBar
            node={{
              uid: selected.uid,
              kind: selected.kind,
              label: selected.label,
            }}
            ids={["open", "explore", "impact", "related", "path", "ask"]}
            compact
            className="mt-3"
          />

          {selected.location && (
            <p className="mt-2 truncate border-t border-[var(--color-border)] pt-2 text-[11px] text-[var(--color-text-muted)]">
              {compactLocation(selected.location)}
            </p>
          )}
      </div>
    </aside>
  );
}
