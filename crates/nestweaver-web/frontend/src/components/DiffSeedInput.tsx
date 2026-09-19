import { useEffect, useRef, useState } from "react";
import { useStore } from "../stores";
import { api } from "../api/client";

export function DiffSeedInput() {
  const [seedsB, setSeedsB] = useState("");
  const [pending, setPending] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const snapshotA = useStore((s) => s.diffState.snapshotA);
  const workspaceId = useStore((s) => s.activeWorkspaceId);
  const controllerRef = useRef<AbortController | null>(null);
  const setDiffB = useStore((s) => s.setDiffB);
  const clearDiff = useStore((s) => s.clearDiff);
  useEffect(() => () => controllerRef.current?.abort(), []);

  const handleSubmit = async () => {
    const seeds = seedsB
      .split(",")
      .map((s) => s.trim())
      .filter(Boolean);
    if (seeds.length === 0 || !snapshotA || pending) return;
    const controller = new AbortController();
    controllerRef.current?.abort();
    controllerRef.current = controller;
    const current = () => {
      const state = useStore.getState();
      return !controller.signal.aborted && state.diffActive &&
        state.diffState.snapshotA === snapshotA && state.activeWorkspaceId === workspaceId;
    };
    setPending(true);
    setError(null);
    try {
      const result = await api.brainContext(seeds, null, "all", workspaceId, controller.signal);
      if (current()) setDiffB(result, seeds);
    } catch (error) {
      if (current()) setError(error instanceof Error ? error.message : "Context comparison request failed");
    } finally {
      if (current()) setPending(false);
    }
  };

  return (
    <div role="dialog" aria-label="Compare context" className="absolute top-1/2 left-1/2 -translate-x-1/2 -translate-y-1/2 z-50 bg-[var(--color-surface)] border border-[var(--color-border)] rounded-lg shadow-xl p-4 min-w-80">
      <h3 className="text-sm font-semibold mb-2">Compare context</h3>
      <p className="text-xs text-[var(--color-text-muted)] mb-3">
        {!snapshotA ? "Loading the first context…" : pending ? "Loading the second context…" : "Enter seed set B to compare against current context."}
      </p>
      {error && <p role="alert" className="mb-3 text-xs text-red-400">{error}</p>}
      <div className="flex gap-2">
        <input
          type="text"
          value={seedsB}
          aria-label="Second context seeds"
          disabled={!snapshotA || pending}
          onChange={(e) => setSeedsB(e.target.value)}
          onKeyDown={(e) => e.key === "Enter" && handleSubmit()}
          placeholder="Comma-separated seeds..."
          className="flex-1 h-8 px-2 text-sm border border-[var(--color-border)] rounded bg-[var(--color-surface)] outline-none focus:ring-2 focus:ring-[var(--color-graph-selection)]"
          autoFocus
        />
        <button
          onClick={handleSubmit}
          disabled={!snapshotA || pending || !seedsB.trim()}
          className="h-8 px-3 text-xs bg-[var(--color-graph-selection)] text-white rounded opacity-90 hover:opacity-100"
        >
          Compare
        </button>
      </div>
      <button
        onClick={() => { controllerRef.current?.abort(); clearDiff(); }}
        className="mt-2 text-xs text-[var(--color-text-muted)] hover:text-[var(--color-text)]"
      >
        Cancel
      </button>
    </div>
  );
}
