import { useStore } from "../../stores";
import type { GraphMode } from "../../api/types";

const modes: { key: GraphMode; label: string }[] = [
  { key: "overview", label: "Overview" },
  { key: "context", label: "Context" },
  { key: "impact", label: "Impact" },
  { key: "repos", label: "Repos" },
  { key: "features", label: "Features" },
  { key: "local", label: "Local" },
];

export function ModeTabs() {
  const graphMode = useStore((s) => s.graphMode);
  const setGraphMode = useStore((s) => s.setGraphMode);

  return (
    <div
      role="group"
      aria-label="Graph mode"
      className="flex border-b border-[var(--color-border)] bg-[var(--color-surface)] shrink-0"
    >
      {modes.map((m) => (
        <button
          key={m.key}
          type="button"
          aria-pressed={graphMode === m.key}
          onClick={() => setGraphMode(m.key)}
          className={`flex-1 px-3 py-2 text-xs font-medium transition-colors ${
            graphMode === m.key
              ? "border-b-2 border-[var(--color-graph-selection)] text-[var(--color-graph-selection)]"
              : "border-b-2 border-transparent text-[var(--color-text-muted)] hover:text-[var(--color-text)]"
          }`}
        >
          {m.label}
        </button>
      ))}
    </div>
  );
}
