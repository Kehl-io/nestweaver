import { useState } from "react";
import { useStore } from "../stores";
import { api } from "../api/client";

export function DiffSeedInput() {
  const [seedsB, setSeedsB] = useState("");
  const setDiffB = useStore((s) => s.setDiffB);
  const clearDiff = useStore((s) => s.clearDiff);

  const handleSubmit = async () => {
    const seeds = seedsB
      .split(",")
      .map((s) => s.trim())
      .filter(Boolean);
    if (seeds.length === 0) return;
    try {
      const result = await api.brainContext(seeds, 2000, "all");
      setDiffB(result, seeds);
    } catch {
      console.error("Diff B failed");
    }
  };

  return (
    <div className="absolute top-1/2 left-1/2 -translate-x-1/2 -translate-y-1/2 z-50 bg-[var(--color-surface)] border border-[var(--color-border)] rounded-lg shadow-xl p-4 min-w-80">
      <h3 className="text-sm font-semibold mb-2">Compare context</h3>
      <p className="text-xs text-[var(--color-text-muted)] mb-3">
        Enter seed set B to compare against current context.
      </p>
      <div className="flex gap-2">
        <input
          type="text"
          value={seedsB}
          onChange={(e) => setSeedsB(e.target.value)}
          onKeyDown={(e) => e.key === "Enter" && handleSubmit()}
          placeholder="Comma-separated seeds..."
          className="flex-1 h-8 px-2 text-sm border border-[var(--color-border)] rounded bg-[var(--color-surface)] outline-none focus:ring-2 focus:ring-blue-500"
          autoFocus
        />
        <button
          onClick={handleSubmit}
          className="h-8 px-3 text-xs bg-blue-500 text-white rounded hover:bg-blue-600"
        >
          Compare
        </button>
      </div>
      <button
        onClick={clearDiff}
        className="mt-2 text-xs text-[var(--color-text-muted)] hover:text-[var(--color-text)]"
      >
        Cancel
      </button>
    </div>
  );
}
