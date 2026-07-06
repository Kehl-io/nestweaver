import { useStore } from "../../stores";

const PATH_COLORS = ["#3B82F6", "#EF4444", "#22C55E", "#F59E0B", "#8B5CF6"];

export function PathDetail() {
  const pathfindingFrom = useStore((s) => s.pathfindingFrom);
  const pathfindingTo = useStore((s) => s.pathfindingTo);
  const pathResults = useStore((s) => s.pathResults);
  const pathStatus = useStore((s) => s.pathStatus);
  const pathError = useStore((s) => s.pathError);
  const selectedPathIndex = useStore((s) => s.selectedPathIndex);
  const selectPath = useStore((s) => s.selectPath);
  const clearPathfinding = useStore((s) => s.clearPathfinding);

  return (
    <div className="p-3 text-sm border-b border-[var(--color-border)]">
      <div className="flex items-center justify-between mb-2">
        <h3 className="font-semibold text-xs uppercase text-[var(--color-text-muted)]">
          Paths ({pathResults.length})
        </h3>
        <button
          onClick={clearPathfinding}
          className="text-xs text-blue-500 hover:underline"
        >
          Clear
        </button>
      </div>
      <p className="mb-2 break-all text-[11px] leading-4 text-[var(--color-text-muted)]">
        {pathfindingFrom}
        {pathfindingTo ? ` -> ${pathfindingTo}` : " -> choose a target"}
      </p>
      {pathStatus === "pending" ? (
        <p className="rounded border border-[var(--color-border)] bg-[var(--color-surface-alt)] px-2 py-1.5 text-xs text-[var(--color-text-muted)]">
          Finding paths...
        </p>
      ) : pathStatus === "error" ? (
        <div className="rounded border border-red-500/30 bg-red-500/10 px-2 py-1.5 text-xs text-red-200">
          <p className="font-medium">Path query failed</p>
          <p className="mt-1 break-words text-red-200/85">
            {pathError ?? "The backend did not return a path result."}
          </p>
        </div>
      ) : pathStatus === "empty" || pathResults.length === 0 ? (
        <p className="rounded border border-[var(--color-border)] bg-[var(--color-surface-alt)] px-2 py-1.5 text-xs text-[var(--color-text-muted)]">
          {pathfindingTo
            ? "No path was found between these nodes."
            : "Choose a target node to find a path."}
        </p>
      ) : (
        pathResults.map((path, i) => (
          <button
            key={i}
            onClick={() => selectPath(i)}
            className={`w-full text-left py-1.5 px-2 rounded text-xs mb-1 ${
              selectedPathIndex === i
                ? "bg-blue-500/10 border border-blue-500/20"
                : "hover:bg-[var(--color-surface-alt)]"
            }`}
          >
            <span
              className="inline-block w-2 h-2 rounded-full mr-1.5"
              style={{
                backgroundColor: PATH_COLORS[i % PATH_COLORS.length],
              }}
            />
            Path {i + 1}: {path.length} hop{path.length !== 1 ? "s" : ""}
          </button>
        ))
      )}
    </div>
  );
}
