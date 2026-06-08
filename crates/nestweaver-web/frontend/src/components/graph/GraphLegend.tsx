import { EDGE_COLORS, kindColor } from "./utils/graphColors";
import { useStore } from "../../stores";

const MODE_HINTS: Record<string, string> = {
  overview: "Size shows overview importance; colors show item kind.",
  context: "Seeds stay central; connected nodes are ranked by relevance.",
  local: "Selected item is pinned; neighbors expand by local depth.",
  impact: "Depth and confidence shape the blast-radius scene.",
  repos: "Repositories, services, and hubs define the architecture view.",
  features: "Feature clusters emphasize related implementation areas.",
};

export function GraphLegend() {
  const graphMode = useStore((s) => s.graphMode);
  const viewMode = useStore((s) => s.viewMode);
  const nodeTypeFilter = useStore((s) => s.nodeTypeFilter);
  const edgeTypeFilter = useStore((s) => s.edgeTypeFilter);
  const isDark = document.documentElement.classList.contains("dark");

  const visibleKinds = Object.entries(nodeTypeFilter)
    .filter(([, visible]) => visible !== false)
    .slice(0, 6);
  const visibleEdges = Object.entries(edgeTypeFilter)
    .filter(([, visible]) => visible !== false)
    .slice(0, 5);

  return (
    <aside
      aria-label="Graph legend"
      className="absolute bottom-14 left-3 z-10 hidden w-[min(340px,calc(100vw-1.5rem))] rounded-md border border-[var(--color-border)] bg-[var(--color-surface)]/95 p-3 text-xs shadow-lg backdrop-blur md:block"
    >
      <div className="flex items-center justify-between gap-2">
        <h2 className="font-semibold text-[var(--color-text)]">Legend</h2>
        <span className="rounded bg-[var(--color-surface-alt)] px-1.5 py-0.5 text-[10px] uppercase text-[var(--color-text-muted)]">
          {viewMode}
        </span>
      </div>
      <p className="mt-1 text-[11px] leading-4 text-[var(--color-text-muted)]">
        {MODE_HINTS[graphMode] ?? "Colors and size follow the active perspective."}
      </p>

      <div className="mt-3 grid grid-cols-2 gap-3">
        <div>
          <p className="mb-1 text-[10px] font-medium uppercase text-[var(--color-text-muted)]">
            Nodes
          </p>
          <div className="space-y-1">
            {visibleKinds.map(([kind]) => (
              <div key={kind} className="flex items-center gap-1.5">
                <span
                  className="h-2.5 w-2.5 rounded-full"
                  style={{ backgroundColor: kindColor(kind, isDark) }}
                />
                <span className="truncate text-[11px] text-[var(--color-text-muted)]">
                  {kind}
                </span>
              </div>
            ))}
          </div>
        </div>
        <div>
          <p className="mb-1 text-[10px] font-medium uppercase text-[var(--color-text-muted)]">
            Edges
          </p>
          <div className="space-y-1">
            {visibleEdges.map(([type]) => (
              <div key={type} className="flex items-center gap-1.5">
                <span
                  className="h-px w-5"
                  style={{ backgroundColor: EDGE_COLORS[type] ?? "#94a3b8" }}
                />
                <span className="truncate text-[11px] text-[var(--color-text-muted)]">
                  {type}
                </span>
              </div>
            ))}
          </div>
        </div>
      </div>
    </aside>
  );
}
