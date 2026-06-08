import type { OverviewLandmark, OverviewResponse } from "../../api/types";
import { useStore } from "../../stores";

interface OverviewCommandShelfProps {
  overview: OverviewResponse | null;
  loading: boolean;
  error: string | null;
  reload: () => void;
}

function firstTarget(overview: OverviewResponse | null): OverviewLandmark | null {
  return overview?.start_here[0] ?? overview?.landmarks[0] ?? null;
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
  const selectedNodeKind = useStore((s) => s.selectedNodeKind);
  const selectNode = useStore((s) => s.selectNode);
  const setSeeds = useStore((s) => s.setSeeds);
  const setGraphMode = useStore((s) => s.setGraphMode);
  const requestSemanticLayout = useStore((s) => s.requestSemanticLayout);

  const fallback = firstTarget(overview);
  const actionTarget =
    selectedNodeId != null
      ? {
          uid: selectedNodeId,
          kind: selectedNodeKind ?? undefined,
        }
      : fallback;

  const exploreTarget = () => {
    if (!actionTarget) return;
    selectNode(actionTarget.uid, actionTarget.kind);
    setSeeds([actionTarget.uid]);
    setGraphMode("local");
  };

  const impactTarget = () => {
    if (!actionTarget) return;
    selectNode(actionTarget.uid, actionTarget.kind);
    setGraphMode("impact");
  };

  return (
    <section
      aria-label="Start Here"
      className="absolute left-3 top-3 z-20 flex max-h-[calc(100%-1.5rem)] w-[min(360px,calc(100vw-1.5rem))] flex-col overflow-hidden rounded-md border border-[var(--color-border)] bg-[var(--color-surface)]/95 shadow-xl backdrop-blur sm:left-4 sm:top-4 sm:w-[340px]"
    >
      <div className="flex items-start justify-between gap-3 border-b border-[var(--color-border)] px-3 py-2.5">
        <div className="min-w-0">
          <h2 className="truncate text-sm font-semibold text-[var(--color-text)]">
            Start Here
          </h2>
          <p className="mt-0.5 truncate text-[11px] text-[var(--color-text-muted)]">
            {overview
              ? `${overview.start_here.length} entry points`
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

      <div className="grid grid-cols-2 gap-1.5 border-b border-[var(--color-border)] px-3 py-2">
        <button
          type="button"
          onClick={exploreTarget}
          disabled={!actionTarget}
          className="rounded bg-blue-600 px-2 py-1.5 text-xs font-medium text-white transition-colors hover:bg-blue-500 disabled:cursor-not-allowed disabled:opacity-50 focus-visible:outline focus-visible:outline-2 focus-visible:outline-[var(--color-graph-selection)]"
        >
          Explore
        </button>
        <button
          type="button"
          onClick={impactTarget}
          disabled={!actionTarget}
          className="rounded border border-[var(--color-border)] px-2 py-1.5 text-xs font-medium text-[var(--color-text)] transition-colors hover:bg-[var(--color-surface-alt)] disabled:cursor-not-allowed disabled:opacity-50 focus-visible:outline focus-visible:outline-2 focus-visible:outline-[var(--color-graph-selection)]"
        >
          Impact
        </button>
      </div>

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

      {!loading && !error && overview && (
        <div className="min-h-0 overflow-y-auto px-2 py-2">
          {overview.start_here.slice(0, 7).map((item) => (
            <button
              key={item.uid}
              type="button"
              onClick={() => selectNode(item.uid, item.kind)}
              className={`w-full rounded px-2 py-2 text-left transition-colors focus-visible:outline focus-visible:outline-2 focus-visible:outline-[var(--color-graph-selection)] ${
                selectedNodeId === item.uid
                  ? "bg-blue-500/10 ring-1 ring-blue-500/40"
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
                <p className="mt-0.5 hidden truncate text-[10px] text-[var(--color-text-muted)] sm:block">
                  {compactLocation(item.location)}
                </p>
              )}
            </button>
          ))}
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
