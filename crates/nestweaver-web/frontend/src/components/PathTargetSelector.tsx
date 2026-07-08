import { useState } from "react";
import { useStore } from "../stores";
import { api } from "../api/client";

export function PathTargetSelector() {
  const [target, setTarget] = useState("");
  const pathfindingFrom = useStore((s) => s.pathfindingFrom);
  const pathStatus = useStore((s) => s.pathStatus);
  const setPathfindingTarget = useStore((s) => s.setPathfindingTarget);
  const setPathResults = useStore((s) => s.setPathResults);
  const setPathError = useStore((s) => s.setPathError);
  const clearPathfinding = useStore((s) => s.clearPathfinding);
  const pending = pathStatus === "pending";

  const handleSubmit = async () => {
    const trimmedTarget = target.trim();
    if (!trimmedTarget || !pathfindingFrom || pending) return;
    const request = setPathfindingTarget(trimmedTarget);
    try {
      const results = await api.paths(pathfindingFrom, trimmedTarget, 5, 10);
      setPathResults(results, request);
    } catch (error) {
      setPathError(
        error instanceof Error && error.message
          ? error.message
          : "Path query failed.",
        request,
      );
    }
  };

  return (
    <div className="absolute top-1/2 left-1/2 -translate-x-1/2 -translate-y-1/2 z-50 bg-[var(--color-surface)] border border-[var(--color-border)] rounded-lg shadow-xl p-4 min-w-72">
      <h3 className="text-sm font-semibold mb-2">Find path to...</h3>
      <p className="text-xs text-[var(--color-text-muted)] mb-3">
        From:{" "}
        <code className="bg-[var(--color-surface-alt)] px-1 rounded">
          {pathfindingFrom}
        </code>
      </p>
      <div className="flex gap-2">
        <input
          type="text"
          value={target}
          onChange={(e) => setTarget(e.target.value)}
          onKeyDown={(e) => e.key === "Enter" && handleSubmit()}
          placeholder="Target node UID or name..."
          className="flex-1 h-8 px-2 text-sm border border-[var(--color-border)] rounded bg-[var(--color-surface)] outline-none focus:ring-2 focus:ring-[var(--color-graph-selection)]"
          disabled={pending}
          autoFocus
        />
        <button
          onClick={handleSubmit}
          disabled={pending}
          className="h-8 px-3 text-xs bg-[var(--color-graph-selection)] text-white rounded opacity-90 hover:opacity-100"
        >
          {pending ? "Finding" : "Find"}
        </button>
      </div>
      <button
        onClick={clearPathfinding}
        className="mt-2 text-xs text-[var(--color-text-muted)] hover:text-[var(--color-text)]"
      >
        Cancel
      </button>
    </div>
  );
}
