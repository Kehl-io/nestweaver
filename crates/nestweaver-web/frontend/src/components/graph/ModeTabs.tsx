import { useStore } from "../../stores";
import type { GraphMode } from "../../api/types";

const modes: { key: GraphMode; label: string; needsSelection?: boolean }[] = [
  { key: "overview", label: "Overview" },
  { key: "context", label: "Context" },
  { key: "impact", label: "Impact", needsSelection: true },
  { key: "repos", label: "Repos" },
  { key: "features", label: "Features" },
  { key: "local", label: "Local", needsSelection: true },
];

export function ModeTabs() {
  const graphMode = useStore((s) => s.graphMode);
  const setGraphMode = useStore((s) => s.setGraphMode);
  const selectedNodeId = useStore((s) => s.selectedNodeId);

  return (
    <div
      role="group"
      aria-label="Graph mode"
      className="flex border-b border-[var(--color-border)] bg-[var(--color-surface)] shrink-0"
    >
      {modes.map((m) => {
        // Impact and Local are node-scoped: without a selection they can't
        // render anything, so previously clicking them silently reverted to
        // Overview (read as "tab switching is broken"). Disable them until a
        // node is selected, with a tooltip explaining why.
        const disabled = Boolean(m.needsSelection) && !selectedNodeId;
        const active = graphMode === m.key;
        return (
          <button
            key={m.key}
            type="button"
            aria-pressed={active}
            aria-disabled={disabled}
            disabled={disabled}
            title={
              disabled ? `Select a node first to use ${m.label} mode` : undefined
            }
            onClick={() => {
              if (!disabled) setGraphMode(m.key);
            }}
            className={`flex-1 px-3 py-2 text-xs font-medium transition-colors ${
              active
                ? "border-b-2 border-[var(--color-graph-selection)] text-[var(--color-graph-selection)]"
                : "border-b-2 border-transparent text-[var(--color-text-muted)] hover:text-[var(--color-text)]"
            } ${disabled ? "cursor-not-allowed opacity-40 hover:text-[var(--color-text-muted)]" : ""}`}
          >
            {m.label}
          </button>
        );
      })}
    </div>
  );
}
