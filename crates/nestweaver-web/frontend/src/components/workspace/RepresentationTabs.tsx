import { Braces, Grid3X3, Network, Table2 } from "lucide-react";
import type { RepresentationMode } from "../../api/p1Types";
import { useStore } from "../../stores";

const tabs: {
  mode: RepresentationMode;
  label: string;
  shortLabel: string;
  icon: typeof Network;
}[] = [
  { mode: "graph", label: "Graph representation", shortLabel: "Graph", icon: Network },
  { mode: "table", label: "Table representation", shortLabel: "Table", icon: Table2 },
  { mode: "matrix", label: "Matrix representation", shortLabel: "Matrix", icon: Grid3X3 },
  { mode: "json", label: "JSON representation", shortLabel: "JSON", icon: Braces },
];

function isActive(
  active: RepresentationMode,
  mode: RepresentationMode,
): boolean {
  return active === mode || (active === "list" && mode === "table");
}

export function RepresentationTabs() {
  const representationMode = useStore((s) => s.representationMode);
  const setRepresentationMode = useStore((s) => s.setRepresentationMode);

  return (
    <div
      role="tablist"
      aria-label="Result representation"
      className="inline-flex min-w-0 rounded border border-[var(--color-border)] bg-[var(--color-surface)] p-0.5"
    >
      {tabs.map(({ mode, label, shortLabel, icon: Icon }) => {
        const active = isActive(representationMode, mode);
        return (
          <button
            key={mode}
            type="button"
            role="tab"
            aria-selected={active}
            title={label}
            onClick={() => setRepresentationMode(mode)}
            className={`inline-flex h-7 min-w-0 items-center gap-1.5 rounded px-2 text-[11px] font-medium outline-none transition-colors focus-visible:ring-2 focus-visible:ring-[var(--color-graph-selection)] ${
              active
                ? "bg-[var(--color-surface-alt)] text-[var(--color-graph-selection)]"
                : "text-[var(--color-text-muted)] hover:bg-[var(--color-surface-alt)] hover:text-[var(--color-text)]"
            }`}
          >
            <Icon className="h-3.5 w-3.5 shrink-0" />
            <span className="hidden sm:inline">{shortLabel}</span>
          </button>
        );
      })}
    </div>
  );
}
