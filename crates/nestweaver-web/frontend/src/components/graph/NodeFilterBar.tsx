import { useStore } from "../../stores";
import { kindColor } from "./utils/graphColors";

const NODE_KINDS = [
  { kind: "Function", label: "fn" },
  { kind: "Class", label: "cls" },
  { kind: "Method", label: "mth" },
  { kind: "Interface", label: "ifc" },
  { kind: "Trait", label: "trt" },
  { kind: "Enum", label: "enm" },
  { kind: "Module", label: "mod" },
  { kind: "Note", label: "note" },
  { kind: "Tag", label: "tag" },
] as const;

const EDGE_TYPES = [
  "calls",
  "imports",
  "extends",
  "implements",
  "includes",
] as const;

const EDGE_COLORS: Record<string, string> = {
  calls: "#9CA3AF",
  imports: "#22C55E",
  extends: "#F97316",
  implements: "#06B6D4",
  includes: "#A78BFA",
};

function hexToRgba(hex: string, alpha: number): string {
  const r = parseInt(hex.slice(1, 3), 16);
  const g = parseInt(hex.slice(3, 5), 16);
  const b = parseInt(hex.slice(5, 7), 16);
  return `rgba(${r}, ${g}, ${b}, ${alpha})`;
}

interface PillProps {
  label: string;
  active: boolean;
  color: string;
  onClick: () => void;
}

function FilterPill({ label, active, color, onClick }: PillProps) {
  return (
    <button
      onClick={onClick}
      aria-pressed={active}
      className="min-w-[44px] h-8 px-2 rounded text-xs font-medium transition-all cursor-pointer flex items-center justify-center border"
      style={
        active
          ? {
              backgroundColor: hexToRgba(color, 0.2),
              borderColor: color,
              color: color,
              borderLeftWidth: "3px",
            }
          : {
              backgroundColor: "transparent",
              borderColor: "var(--color-border)",
              color: "var(--color-text-muted)",
              borderLeftWidth: "3px",
              borderLeftColor: color,
            }
      }
      title={`Toggle ${label} visibility`}
    >
      {label}
    </button>
  );
}

interface BulkButtonProps {
  label: string;
  onClick: () => void;
}

function BulkButton({ label, onClick }: BulkButtonProps) {
  return (
    <button
      onClick={onClick}
      className="min-w-[44px] h-8 px-2 rounded text-xs font-medium transition-all cursor-pointer flex items-center justify-center border border-[var(--color-border)] text-[var(--color-text-muted)] hover:text-[var(--color-text)] hover:border-[var(--color-text-muted)]"
    >
      {label}
    </button>
  );
}

export function NodeFilterBar() {
  const nodeTypeFilter = useStore((s) => s.nodeTypeFilter);
  const edgeTypeFilter = useStore((s) => s.edgeTypeFilter);
  const setNodeTypeFilter = useStore((s) => s.setNodeTypeFilter);
  const setAllNodeTypes = useStore((s) => s.setAllNodeTypes);
  const setEdgeTypeFilter = useStore((s) => s.setEdgeTypeFilter);

  const isDark = document.documentElement.classList.contains("dark");

  return (
    <div className="flex flex-col gap-1 px-2 py-1.5 border-t border-[var(--color-border)] bg-[var(--color-surface-alt)]">
      {/* Row 1: Node types */}
      <div className="flex flex-wrap items-center gap-1">
        <BulkButton label="All" onClick={() => setAllNodeTypes(true)} />
        <BulkButton label="None" onClick={() => setAllNodeTypes(false)} />
        <div className="w-px h-5 bg-[var(--color-border)] mx-0.5" />
        {NODE_KINDS.map(({ kind, label }) => (
          <FilterPill
            key={kind}
            label={label}
            active={nodeTypeFilter[kind] !== false}
            color={kindColor(kind, isDark)}
            onClick={() =>
              setNodeTypeFilter(kind, nodeTypeFilter[kind] === false)
            }
          />
        ))}
      </div>
      {/* Row 2: Edge types */}
      <div className="flex flex-wrap items-center gap-1">
        {EDGE_TYPES.map((type) => (
          <FilterPill
            key={type}
            label={type}
            active={edgeTypeFilter[type] !== false}
            color={EDGE_COLORS[type] ?? "#9CA3AF"}
            onClick={() =>
              setEdgeTypeFilter(type, edgeTypeFilter[type] === false)
            }
          />
        ))}
      </div>
    </div>
  );
}
