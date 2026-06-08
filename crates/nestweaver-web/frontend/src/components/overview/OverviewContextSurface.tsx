import type { OverviewLandmark, OverviewResponse } from "../../api/types";
import { useStore } from "../../stores";

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

const SYMBOL_KINDS = new Set([
  "symbol",
  "Function",
  "Class",
  "Method",
  "Interface",
  "Trait",
  "Enum",
  "Module",
  "Extension",
  "Constant",
  "Property",
  "TypeAlias",
  "Variable",
]);

function isSymbolLandmark(item: OverviewLandmark | null): boolean {
  return item != null && SYMBOL_KINDS.has(item.kind);
}

function canExploreLandmark(item: OverviewLandmark | null): boolean {
  if (item == null) return false;
  return isSymbolLandmark(item) || item.kind === "note";
}

function firstSupportedTarget(
  overview: OverviewResponse | null,
  predicate: (item: OverviewLandmark | null) => boolean,
): OverviewLandmark | null {
  return (
    overview?.start_here.find(predicate) ??
    overview?.landmarks.find(predicate) ??
    null
  );
}

function compactLocation(location: string): string {
  const parts = location.split("/");
  return parts.length > 3 ? parts.slice(-3).join("/") : location;
}

export function OverviewContextSurface({
  overview,
}: OverviewContextSurfaceProps) {
  const selectedNodeId = useStore((s) => s.selectedNodeId);
  const selectedNodeKind = useStore((s) => s.selectedNodeKind);
  const graphInstance = useStore((s) => s.graphInstance);
  const setSeeds = useStore((s) => s.setSeeds);
  const selectNode = useStore((s) => s.selectNode);
  const setGraphMode = useStore((s) => s.setGraphMode);
  const requestSemanticLayout = useStore((s) => s.requestSemanticLayout);
  const viewMode = useStore((s) => s.viewMode);
  const toggleViewMode = useStore((s) => s.toggleViewMode);

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
  const selectedOverviewItem = findOverviewItem(overview, selectedNodeId);
  const hasOverviewSelection =
    selectedNodeId != null && selectedOverviewItem != null;
  const exploreFallback = firstSupportedTarget(overview, canExploreLandmark);
  const impactFallback = firstSupportedTarget(overview, isSymbolLandmark);
  const exploreActionTarget =
    hasOverviewSelection
      ? canExploreLandmark(selectedOverviewItem) ? selectedOverviewItem : null
      : exploreFallback;
  const impactActionTarget =
    hasOverviewSelection
      ? isSymbolLandmark(selectedOverviewItem) ? selectedOverviewItem : null
      : impactFallback;

  const exploreTarget = () => {
    if (!exploreActionTarget) return;
    selectNode(exploreActionTarget.uid, exploreActionTarget.kind);
    setSeeds([exploreActionTarget.uid]);
    setGraphMode("local");
  };

  const impactTarget = () => {
    if (!impactActionTarget) return;
    selectNode(impactActionTarget.uid, impactActionTarget.kind);
    setGraphMode("impact");
  };

  return (
    <aside
      aria-label="Overview context"
      className="absolute bottom-3 right-3 z-20 max-h-[min(420px,calc(100%-1.5rem))] w-[min(390px,calc(100vw-1.5rem))] overflow-hidden rounded-md border border-[var(--color-border)] bg-[var(--color-surface)]/95 shadow-xl backdrop-blur sm:bottom-4 sm:right-4"
    >
      {selected ? (
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
              <span className="shrink-0 rounded bg-blue-500/10 px-1.5 py-0.5 text-[10px] font-medium text-blue-600">
                Overview
              </span>
            )}
          </div>

          <p className="mt-2 max-h-12 overflow-hidden text-xs leading-5 text-[var(--color-text-muted)]">
            {selected.reason ?? "Selected overview landmark"}
          </p>

          {selected.location && (
            <p className="mt-2 truncate border-t border-[var(--color-border)] pt-2 text-[11px] text-[var(--color-text-muted)]">
              {compactLocation(selected.location)}
            </p>
          )}

          <div className="mt-3 grid grid-cols-2 gap-1.5">
            <button
              type="button"
              onClick={exploreTarget}
              disabled={!exploreActionTarget}
              title={!exploreActionTarget ? "Explore is available for symbols and notes" : undefined}
              className="rounded bg-blue-600 px-2 py-1.5 text-xs font-medium text-white transition-colors hover:bg-blue-500 disabled:cursor-not-allowed disabled:opacity-50 focus-visible:outline focus-visible:outline-2 focus-visible:outline-[var(--color-graph-selection)]"
            >
              Explore neighborhood
            </button>
            <button
              type="button"
              onClick={impactTarget}
              disabled={!impactActionTarget}
              title={!impactActionTarget ? "Impact is available for symbols" : undefined}
              className="rounded border border-[var(--color-border)] px-2 py-1.5 text-xs font-medium text-[var(--color-text)] transition-colors hover:bg-[var(--color-surface-alt)] disabled:cursor-not-allowed disabled:opacity-50 focus-visible:outline focus-visible:outline-2 focus-visible:outline-[var(--color-graph-selection)]"
            >
              Impact
            </button>
            <button
              type="button"
              onClick={requestSemanticLayout}
              className="rounded border border-[var(--color-border)] px-2 py-1.5 text-xs font-medium text-[var(--color-text-muted)] transition-colors hover:bg-[var(--color-surface-alt)] hover:text-[var(--color-text)] focus-visible:outline focus-visible:outline-2 focus-visible:outline-[var(--color-graph-selection)]"
            >
              Settle layout
            </button>
            <button
              type="button"
              onClick={toggleViewMode}
              className="rounded border border-[var(--color-border)] px-2 py-1.5 text-xs font-medium text-[var(--color-text-muted)] transition-colors hover:bg-[var(--color-surface-alt)] hover:text-[var(--color-text)] focus-visible:outline focus-visible:outline-2 focus-visible:outline-[var(--color-graph-selection)]"
            >
              {viewMode === "list" ? "Graph view" : "List view"}
            </button>
          </div>
        </div>
      ) : (
        <div className="p-3">
          <div className="flex items-start justify-between gap-3">
            <div>
              <h2 className="text-sm font-semibold text-[var(--color-text)]">
                Overview Map
              </h2>
              {overview && (
                <p className="mt-0.5 text-[11px] text-[var(--color-text-muted)]">
                  {overview.landmarks.length} landmarks
                </p>
              )}
            </div>
            {overview && overview.gaps.length > 0 && (
              <span className="rounded bg-amber-500/10 px-1.5 py-0.5 text-[10px] font-medium text-amber-600">
                {overview.gaps.length} gap{overview.gaps.length === 1 ? "" : "s"}
              </span>
            )}
          </div>

          {overview ? (
            <>
              <div className="mt-3 grid grid-cols-3 divide-x divide-[var(--color-border)] border-y border-[var(--color-border)] py-2 text-center">
                <div className="px-2">
                  <p className="text-sm font-semibold text-[var(--color-text)]">
                    {overview.counts.repo_count}
                  </p>
                  <p className="text-[10px] uppercase text-[var(--color-text-muted)]">
                    Repos
                  </p>
                </div>
                <div className="px-2">
                  <p className="text-sm font-semibold text-[var(--color-text)]">
                    {overview.counts.symbol_count}
                  </p>
                  <p className="text-[10px] uppercase text-[var(--color-text-muted)]">
                    Symbols
                  </p>
                </div>
                <div className="px-2">
                  <p className="text-sm font-semibold text-[var(--color-text)]">
                    {overview.counts.note_count}
                  </p>
                  <p className="text-[10px] uppercase text-[var(--color-text-muted)]">
                    Notes
                  </p>
                </div>
              </div>

              {overview.gaps[0] && (
                <div className="mt-3 border-t border-[var(--color-border)] pt-2">
                  <p className="truncate text-xs font-medium text-[var(--color-text)]">
                    {overview.gaps[0].label}
                  </p>
                  <p className="mt-1 max-h-10 overflow-hidden text-[11px] leading-5 text-[var(--color-text-muted)]">
                    {overview.gaps[0].detail}
                  </p>
                </div>
              )}

              <div className="mt-3 grid grid-cols-3 gap-1.5">
                <button
                  type="button"
                  onClick={exploreTarget}
                  disabled={!exploreActionTarget}
                  title={!exploreActionTarget ? "Explore is available for symbols and notes" : undefined}
                  className="rounded bg-blue-600 px-2 py-1.5 text-xs font-medium text-white transition-colors hover:bg-blue-500 disabled:cursor-not-allowed disabled:opacity-50 focus-visible:outline focus-visible:outline-2 focus-visible:outline-[var(--color-graph-selection)]"
                >
                  Explore
                </button>
                <button
                  type="button"
                  onClick={impactTarget}
                  disabled={!impactActionTarget}
                  title={!impactActionTarget ? "Impact is available for symbols" : undefined}
                  className="rounded border border-[var(--color-border)] px-2 py-1.5 text-xs font-medium text-[var(--color-text)] transition-colors hover:bg-[var(--color-surface-alt)] disabled:cursor-not-allowed disabled:opacity-50 focus-visible:outline focus-visible:outline-2 focus-visible:outline-[var(--color-graph-selection)]"
                >
                  Impact
                </button>
                <button
                  type="button"
                  onClick={requestSemanticLayout}
                  className="rounded border border-[var(--color-border)] px-2 py-1.5 text-xs font-medium text-[var(--color-text-muted)] transition-colors hover:bg-[var(--color-surface-alt)] hover:text-[var(--color-text)] focus-visible:outline focus-visible:outline-2 focus-visible:outline-[var(--color-graph-selection)]"
                >
                  Settle
                </button>
              </div>
            </>
          ) : (
            <p className="mt-2 text-xs text-[var(--color-text-muted)]">
              Open an indexed project to load landmarks.
            </p>
          )}
        </div>
      )}
    </aside>
  );
}
