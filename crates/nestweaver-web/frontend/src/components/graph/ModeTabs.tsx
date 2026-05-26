import { useStore } from "../../stores";
import type { GraphMode } from "../../api/types";

const modes: { key: GraphMode; label: string }[] = [
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
    <div className="flex border-t border-[var(--color-border)] bg-[var(--color-surface)] shrink-0">
      {modes.map((m) => (
        <button
          key={m.key}
          type="button"
          onClick={() => setGraphMode(m.key)}
          className={`flex-1 px-3 py-2 text-xs font-medium transition-colors ${
            graphMode === m.key
              ? "border-t-2 border-blue-500 text-blue-600"
              : "border-t-2 border-transparent text-[var(--color-text-muted)] hover:text-[var(--color-text)]"
          }`}
        >
          {m.label}
        </button>
      ))}
    </div>
  );
}
