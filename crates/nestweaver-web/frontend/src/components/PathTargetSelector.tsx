import { useState } from "react";
import { useStore } from "../stores";
import { api } from "../api/client";

export function PathTargetSelector() {
  const [target, setTarget] = useState("");
  const pathfindingFrom = useStore((s) => s.pathfindingFrom);
  const setPathfindingTarget = useStore((s) => s.setPathfindingTarget);
  const setPathResults = useStore((s) => s.setPathResults);
  const clearPathfinding = useStore((s) => s.clearPathfinding);

  const handleSubmit = async () => {
    if (!target || !pathfindingFrom) return;
    setPathfindingTarget(target);
    try {
      const results = await api.paths(pathfindingFrom, target, 5, 10);
      setPathResults(results as any[]);
    } catch {
      setPathResults([]);
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
          className="flex-1 h-8 px-2 text-sm border border-[var(--color-border)] rounded bg-[var(--color-surface)] outline-none focus:ring-2 focus:ring-blue-500"
          autoFocus
        />
        <button
          onClick={handleSubmit}
          className="h-8 px-3 text-xs bg-blue-500 text-white rounded hover:bg-blue-600"
        >
          Find
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
