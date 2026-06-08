import { useStore } from "../../stores";

export function ActiveFilterSummary() {
  const nodeTypeFilter = useStore((s) => s.nodeTypeFilter);
  const edgeTypeFilter = useStore((s) => s.edgeTypeFilter);
  const scopeFilter = useStore((s) => s.scopeFilter);
  const graphMode = useStore((s) => s.graphMode);
  const viewMode = useStore((s) => s.viewMode);

  const hiddenNodeTypes = Object.entries(nodeTypeFilter)
    .filter(([, visible]) => visible === false)
    .map(([kind]) => kind);
  const hiddenEdgeTypes = Object.entries(edgeTypeFilter)
    .filter(([, visible]) => visible === false)
    .map(([type]) => type);

  const hasFilters =
    hiddenNodeTypes.length > 0 ||
    hiddenEdgeTypes.length > 0 ||
    scopeFilter !== "all";

  return (
    <div className="flex min-h-9 items-center gap-2 border-t border-[var(--color-border)] bg-[var(--color-surface-alt)] px-3 py-1 text-[11px] text-[var(--color-text-muted)]">
      <span className="font-medium text-[var(--color-text)]">
        {graphMode} / {viewMode}
      </span>
      {hasFilters ? (
        <>
          {scopeFilter !== "all" && (
            <span className="rounded border border-[var(--color-border)] px-1.5 py-0.5">
              Scope: {scopeFilter.replace("_", " ")}
            </span>
          )}
          {hiddenNodeTypes.length > 0 && (
            <span className="rounded border border-[var(--color-border)] px-1.5 py-0.5">
              Hidden nodes: {hiddenNodeTypes.join(", ")}
            </span>
          )}
          {hiddenEdgeTypes.length > 0 && (
            <span className="rounded border border-[var(--color-border)] px-1.5 py-0.5">
              Hidden edges: {hiddenEdgeTypes.join(", ")}
            </span>
          )}
        </>
      ) : (
        <span>No active filters</span>
      )}
    </div>
  );
}
