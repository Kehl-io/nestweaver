import { EDGE_COLORS, kindColor } from "./utils/graphColors";
import { perspectiveVisuals } from "./utils/perspectiveVisuals";
import { useStore } from "../../stores";

export function GraphLegend() {
  const graphMode = useStore((s) => s.graphMode);
  const viewMode = useStore((s) => s.viewMode);
  const nodeTypeFilter = useStore((s) => s.nodeTypeFilter);
  const edgeTypeFilter = useStore((s) => s.edgeTypeFilter);
  const isDark = document.documentElement.classList.contains("dark");
  const visuals = perspectiveVisuals(graphMode);

  const visibleKinds = Object.entries(nodeTypeFilter)
    .filter(([, visible]) => visible !== false)
    .slice(0, 6);
  const visibleEdges =
    graphMode === "overview"
      ? [["overview", true] as const]
      : Object.entries(edgeTypeFilter)
          .filter(([, visible]) => visible !== false)
          .slice(0, 5);

  return (
    <aside
      aria-label="Graph legend"
      className="pointer-events-none absolute bottom-14 left-3 right-3 z-10 hidden items-center gap-2 text-xs md:flex"
    >
      <div className="flex max-w-full flex-wrap items-center gap-1.5 rounded-md border border-[var(--color-border)] bg-[var(--color-surface)]/78 px-2 py-1.5 shadow-sm backdrop-blur-xl">
        <span className="rounded bg-[var(--color-surface-alt)] px-1.5 py-0.5 text-[10px] uppercase text-[var(--color-text-muted)]">
          {viewMode}
        </span>
        <span className="rounded border border-[var(--color-border)] px-1.5 py-0.5 text-[10px] text-[var(--color-text-muted)]">
          {visuals.sizeEncoding}
        </span>
        <span className="rounded border border-[var(--color-border)] px-1.5 py-0.5 text-[10px] text-[var(--color-text-muted)]">
          {visuals.colorEncoding}
        </span>
        {visibleKinds.slice(0, 4).map(([kind]) => (
          <span key={kind} className="inline-flex items-center gap-1.5 text-[10px] text-[var(--color-text-muted)]">
            <span
              className="h-2 w-2 rounded-full"
              style={{ backgroundColor: kindColor(kind, isDark) }}
            />
            {kind}
          </span>
        ))}
        {visibleEdges.slice(0, 3).map(([type]) => (
          <span key={type} className="inline-flex items-center gap-1.5 text-[10px] text-[var(--color-text-muted)]">
            <span
              className="h-px w-4"
              style={{ backgroundColor: EDGE_COLORS[type] ?? "#94a3b8" }}
            />
            {type}
          </span>
        ))}
      </div>
    </aside>
  );
}
