import { useState } from "react";
import { useStore } from "../../stores";
import { api } from "../../api/client";
import type { GapItem } from "../../stores/analysisSlice";
import { ExportMenu } from "../export/ExportMenu";

export function GraphToolbar() {
  const communityOverlay = useStore((s) => s.communityOverlay);
  const tagsVisible = useStore((s) => s.tagsVisible);
  const minimapVisible = useStore((s) => s.minimapVisible);
  const toggleCommunity = useStore((s) => s.toggleCommunityOverlay);
  const toggleTags = useStore((s) => s.toggleTags);
  const toggleMinimap = useStore((s) => s.toggleMinimap);

  const requestSemanticLayout = useStore((s) => s.requestSemanticLayout);

  const [exportOpen, setExportOpen] = useState(false);

  const seeds = useStore((s) => s.seeds);
  const startDiff = useStore((s) => s.startDiff);
  const setGapItems = useStore((s) => s.setGapItems);
  const toggleGapPanel = useStore((s) => s.toggleGapPanel);
  const gapActive = useStore((s) => s.gapActive);

  const buttons = [
    { label: "C", title: "Toggle community detection (c)", active: communityOverlay, onClick: toggleCommunity },
    { label: "#", title: "Toggle tag nodes (t)", active: tagsVisible, onClick: toggleTags },
    { label: "M", title: "Toggle minimap (m)", active: minimapVisible, onClick: toggleMinimap },
  ];

  return (
    <div className="absolute top-2 right-2 z-10 flex flex-col gap-1">
      {buttons.map((b) => (
        <button
          key={b.label}
          onClick={b.onClick}
          title={b.title}
          className={`flex h-8 w-8 items-center justify-center rounded border text-xs font-mono transition-colors ${
            b.active
              ? "border-blue-300 bg-blue-100 text-blue-700"
              : "border-[var(--color-border)] bg-[var(--color-surface)] text-[var(--color-text-muted)] hover:bg-[var(--color-surface-alt)]"
          }`}
        >
          {b.label}
        </button>
      ))}

      {/* UMAP semantic layout */}
      <button
        onClick={requestSemanticLayout}
        title="Semantic layout (UMAP)"
        className="flex h-8 w-8 items-center justify-center rounded border border-[var(--color-border)] bg-[var(--color-surface)] text-[10px] font-medium text-[var(--color-text-muted)] hover:bg-[var(--color-surface-alt)]"
      >
        U
      </button>

      {/* Separator */}
      <div className="h-px bg-[var(--color-border)] my-1" />

      {/* Compare button */}
      <button
        onClick={async () => {
          try {
            const result = await api.brainContext(seeds, 2000, "all");
            startDiff(result, seeds);
          } catch {
            /* ignore */
          }
        }}
        title="Compare contexts"
        className="flex h-8 w-8 items-center justify-center rounded border border-[var(--color-border)] bg-[var(--color-surface)] text-[10px] font-medium text-[var(--color-text-muted)] hover:bg-[var(--color-surface-alt)]"
      >
        Diff
      </button>

      {/* Gaps button */}
      <button
        onClick={async () => {
          try {
            const report = await api.gaps();
            const items: GapItem[] = [
              ...report.undocumented.map((m) => ({
                type: "undocumented" as const,
                label: m.module,
                detail: `${m.symbol_count} symbols with no documentation`,
                nodeUids: [] as string[],
              })),
              ...report.untested.map((uid) => ({
                type: "untested" as const,
                label: uid.split(":").pop() || uid,
                detail: "Entry point with no test coverage",
                nodeUids: [uid],
              })),
            ];
            setGapItems(items);
            if (!gapActive) toggleGapPanel();
          } catch {
            console.error("Gap analysis failed");
          }
        }}
        title="Analyze structural gaps"
        className={`flex h-8 w-8 items-center justify-center rounded border text-[10px] font-medium ${
          gapActive
            ? "border-amber-300 bg-amber-100 text-amber-700"
            : "border-[var(--color-border)] bg-[var(--color-surface)] text-[var(--color-text-muted)] hover:bg-[var(--color-surface-alt)]"
        }`}
      >
        Gap
      </button>

      {/* Separator */}
      <div className="h-px bg-[var(--color-border)] my-1" />

      {/* Export button */}
      <div className="relative">
        <button
          onClick={() => setExportOpen(!exportOpen)}
          title="Export graph (e)"
          className="flex h-8 w-8 items-center justify-center rounded border border-[var(--color-border)] bg-[var(--color-surface)] text-[10px] font-medium text-[var(--color-text-muted)] hover:bg-[var(--color-surface-alt)]"
        >
          ↓
        </button>
        {exportOpen && <ExportMenu onClose={() => setExportOpen(false)} />}
      </div>
    </div>
  );
}
