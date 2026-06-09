import type { OverviewLandmark, OverviewResponse } from "../../api/types";
import { useStore } from "../../stores";

interface OverviewCommandShelfProps {
  overview: OverviewResponse | null;
  loading: boolean;
  error: string | null;
  reload: () => void;
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

function findSelectedLandmark(
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

function isEmptyOverview(overview: OverviewResponse | null): boolean {
  return (
    overview != null &&
    overview.counts.repo_count === 0 &&
    overview.counts.service_count === 0 &&
    overview.counts.vault_count === 0 &&
    overview.counts.symbol_count === 0 &&
    overview.counts.note_count === 0 &&
    overview.start_here.length === 0 &&
    overview.landmarks.length === 0
  );
}

function compactLocation(location: string): string {
  const parts = location.split("/");
  return parts.length > 2 ? parts.slice(-2).join("/") : location;
}

export function OverviewCommandShelf({
  overview,
  loading,
  error,
  reload,
}: OverviewCommandShelfProps) {
  const selectedNodeId = useStore((s) => s.selectedNodeId);
  const selectNode = useStore((s) => s.selectNode);
  const setSeeds = useStore((s) => s.setSeeds);
  const setGraphMode = useStore((s) => s.setGraphMode);
  const requestSemanticLayout = useStore((s) => s.requestSemanticLayout);

  const emptyOverview = isEmptyOverview(overview);
  const selectedLandmark = findSelectedLandmark(overview, selectedNodeId);
  const hasOverviewSelection = selectedNodeId != null && selectedLandmark != null;
  const exploreFallback = firstSupportedTarget(overview, canExploreLandmark);
  const impactFallback = firstSupportedTarget(overview, isSymbolLandmark);
  const exploreActionTarget =
    hasOverviewSelection
      ? canExploreLandmark(selectedLandmark) ? selectedLandmark : null
      : exploreFallback;
  const impactActionTarget =
    hasOverviewSelection
      ? isSymbolLandmark(selectedLandmark) ? selectedLandmark : null
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
    <section
      aria-label="Start Here"
      className="absolute left-3 top-3 z-30 flex max-h-[min(310px,calc(100%-5rem))] w-[min(248px,calc(100vw-1.5rem))] flex-col overflow-hidden rounded-md border border-[var(--color-border)] bg-[var(--color-surface)]/86 shadow-md backdrop-blur-xl sm:left-4 sm:top-4"
    >
      <div className="flex items-start justify-between gap-3 px-3 py-2.5">
        <div className="min-w-0">
          <h2 className="truncate text-sm font-semibold text-[var(--color-text)]">
            Start Here
          </h2>
          <p className="mt-0.5 truncate text-[11px] text-[var(--color-text-muted)]">
            {overview
              ? emptyOverview
                ? "No indexed content"
                : `${overview.start_here.length} entry points`
              : loading
                ? "Loading overview"
                : "Overview unavailable"}
          </p>
        </div>
        <div className="flex shrink-0 items-center gap-1.5">
          <button
            type="button"
            onClick={requestSemanticLayout}
            className="rounded border border-[var(--color-border)] px-2 py-1 text-[11px] font-medium text-[var(--color-text-muted)] transition-colors hover:bg-[var(--color-surface-alt)] hover:text-[var(--color-text)] focus-visible:outline focus-visible:outline-2 focus-visible:outline-[var(--color-graph-selection)]"
          >
            Settle
          </button>
          <button
            type="button"
            onClick={reload}
            className="rounded border border-[var(--color-border)] px-2 py-1 text-[11px] font-medium text-[var(--color-text-muted)] transition-colors hover:bg-[var(--color-surface-alt)] hover:text-[var(--color-text)] focus-visible:outline focus-visible:outline-2 focus-visible:outline-[var(--color-graph-selection)]"
          >
            Refresh
          </button>
        </div>
      </div>

      {!emptyOverview && (
        <div className="space-y-2 border-t border-[var(--color-border)] px-3 py-2">
          {overview && (
            <div className="grid grid-cols-3 divide-x divide-[var(--color-border)] rounded border border-[var(--color-border)] bg-[var(--color-surface-alt)]/65 py-1.5 text-center">
              <div className="px-1.5">
                <p className="text-xs font-semibold text-[var(--color-text)]">
                  {overview.counts.repo_count}
                </p>
                <p className="text-[9px] uppercase text-[var(--color-text-muted)]">
                  Repos
                </p>
              </div>
              <div className="px-1.5">
                <p className="text-xs font-semibold text-[var(--color-text)]">
                  {overview.counts.symbol_count}
                </p>
                <p className="text-[9px] uppercase text-[var(--color-text-muted)]">
                  Symbols
                </p>
              </div>
              <div className="px-1.5">
                <p className="text-xs font-semibold text-[var(--color-text)]">
                  {overview.gaps.length}
                </p>
                <p className="text-[9px] uppercase text-[var(--color-text-muted)]">
                  Gaps
                </p>
              </div>
            </div>
          )}
          <div className="grid grid-cols-2 gap-1.5">
            <button
              type="button"
              onClick={exploreTarget}
              disabled={!exploreActionTarget}
              title={!exploreActionTarget ? "Explore is available for symbols and notes" : undefined}
              className="rounded bg-[var(--color-graph-selection)] px-2 py-1.5 text-xs font-medium text-white transition-opacity hover:opacity-90 disabled:cursor-not-allowed disabled:opacity-50 focus-visible:outline focus-visible:outline-2 focus-visible:outline-[var(--color-graph-selection)]"
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
          </div>
        </div>
      )}

      {loading && (
        <div className="px-3 py-3 text-xs text-[var(--color-text-muted)]">
          Loading overview...
        </div>
      )}

      {error && (
        <div className="space-y-2 px-3 py-3">
          <p className="line-clamp-2 text-xs text-red-500">{error}</p>
          <button
            type="button"
            onClick={reload}
            className="rounded border border-red-300 px-2 py-1 text-xs font-medium text-red-600 transition-colors hover:bg-red-50 focus-visible:outline focus-visible:outline-2 focus-visible:outline-[var(--color-graph-selection)]"
          >
            Retry overview
          </button>
        </div>
      )}

      {!loading && !error && emptyOverview && (
        <div className="space-y-3 px-3 py-3 text-xs text-[var(--color-text-muted)]">
          <p className="text-[var(--color-text)]">
            Index a project or add a vault to build the overview map.
          </p>
          <div className="space-y-1.5 rounded border border-[var(--color-border)] bg-[var(--color-surface-alt)] p-2 font-mono text-[11px]">
            <p>nestweaver index --repo .</p>
            <p>nestweaver brain add ~/vault --name personal</p>
          </div>
          <button
            type="button"
            onClick={reload}
            className="rounded border border-[var(--color-border)] px-2 py-1 text-xs font-medium text-[var(--color-text)] transition-colors hover:bg-[var(--color-surface-alt)] focus-visible:outline focus-visible:outline-2 focus-visible:outline-[var(--color-graph-selection)]"
          >
            Retry overview
          </button>
        </div>
      )}

      {!loading && !error && overview && !emptyOverview && (
        <div className="min-h-0 space-y-1 overflow-y-auto border-t border-[var(--color-border)] px-2 py-2">
          {overview.start_here.slice(0, 2).map((item) => (
            <button
              key={item.uid}
              type="button"
              onClick={() => selectNode(item.uid, item.kind)}
              className={`w-full rounded px-2 py-1.5 text-left transition-colors focus-visible:outline focus-visible:outline-2 focus-visible:outline-[var(--color-graph-selection)] ${
                selectedNodeId === item.uid
                  ? "bg-[var(--color-surface-alt)] ring-1 ring-[var(--color-graph-selection)]"
                  : "hover:bg-[var(--color-surface-alt)]"
              }`}
            >
              <div className="flex min-w-0 items-center gap-2">
                <span className="shrink-0 rounded bg-[var(--color-surface-alt)] px-1.5 py-0.5 text-[10px] font-medium uppercase text-[var(--color-text-muted)]">
                  {item.kind}
                </span>
                <span className="min-w-0 flex-1 truncate text-xs font-semibold text-[var(--color-text)]">
                  {item.label}
                </span>
              </div>
              <p className="mt-1 line-clamp-2 text-[11px] leading-4 text-[var(--color-text-muted)]">
                {item.reason}
              </p>
              {item.location && (
                <p className="mt-0.5 truncate text-[10px] text-[var(--color-text-muted)]">
                  {compactLocation(item.location)}
                </p>
              )}
            </button>
          ))}
          {overview.start_here.length > 2 && (
            <p className="px-2 pt-0.5 text-[10px] text-[var(--color-text-muted)]">
              {overview.start_here.length - 2} more entry points in search
            </p>
          )}
          {overview.start_here.length === 0 && (
            <p className="px-1 py-2 text-xs text-[var(--color-text-muted)]">
              No entry points found.
            </p>
          )}
        </div>
      )}
    </section>
  );
}
