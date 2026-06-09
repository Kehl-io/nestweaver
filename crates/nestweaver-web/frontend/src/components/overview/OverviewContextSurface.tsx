import type { OverviewLandmark, OverviewResponse } from "../../api/types";
import { useStore } from "../../stores";
import { NodeActionBar } from "../actions/NodeActionBar";

interface OverviewContextSurfaceProps {
  overview: OverviewResponse | null;
}

function findOverviewItem(
  overview: OverviewResponse | null,
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
            {overviewItem && (
              <span className="shrink-0 rounded bg-[var(--color-surface-alt)] px-1.5 py-0.5 text-[10px] font-medium text-[var(--color-graph-selection)]">
                Overview
              </span>
            )}
          </div>

          <p className="mt-2 max-h-12 overflow-hidden text-xs leading-5 text-[var(--color-text-muted)]">
            {selected.reason ?? "Selected overview landmark"}
          </p>

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
