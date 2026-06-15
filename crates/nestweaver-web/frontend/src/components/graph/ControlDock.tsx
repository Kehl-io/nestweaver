import { useState, type ReactNode } from "react";
import {
  Activity,
  GitCompare,
  Grid3X3,
  Layers3,
  List,
  Map,
  Maximize,
  Network,
  Route,
  Settings,
  Sparkles,
  Tags,
} from "lucide-react";
import { api, loadGapItems } from "../../api/client";
import type { ScopeFilter } from "../../api/types";
import { useStore } from "../../stores";
import { ExportMenu } from "../export/ExportMenu";
import { ForceControls } from "./ForceControls";
import { NodeFilterBar } from "./NodeFilterBar";
import { StyleRules } from "./StyleRules";

function MenuButton({
  active,
  children,
  onClick,
}: {
  active?: boolean;
  children: ReactNode;
  onClick: () => void;
}) {
  return (
    <button
      type="button"
      onClick={onClick}
      className={`inline-flex h-8 items-center gap-1.5 rounded border px-2 text-xs font-medium transition-colors ${
        active
          ? "border-[var(--color-graph-selection)] bg-[var(--color-surface-alt)] text-[var(--color-graph-selection)]"
          : "border-[var(--color-border)] text-[var(--color-text-muted)] hover:bg-[var(--color-surface-alt)] hover:text-[var(--color-text)]"
      }`}
    >
      {children}
    </button>
  );
}

export function ControlDock() {
  const [open, setOpen] = useState(false);
  const viewMode = useStore((s) => s.viewMode);
  const setViewMode = useStore((s) => s.setViewMode);
  const minimapVisible = useStore((s) => s.minimapVisible);
  const toggleMinimap = useStore((s) => s.toggleMinimap);
  const reducedEffects = useStore((s) => s.reducedEffects);
  const toggleReducedEffects = useStore((s) => s.toggleReducedEffects);
  const layoutMode = useStore((s) => s.layoutMode);
  const setLayoutMode = useStore((s) => s.setLayoutMode);
  const communityOverlay = useStore((s) => s.communityOverlay);
  const toggleCommunity = useStore((s) => s.toggleCommunityOverlay);
  const tagsVisible = useStore((s) => s.tagsVisible);
  const toggleTags = useStore((s) => s.toggleTags);
  const scopeFilter = useStore((s) => s.scopeFilter);
  const setScopeFilter = useStore((s) => s.setScopeFilter);
  const requestSemanticLayout = useStore((s) => s.requestSemanticLayout);
  const selectedNodeId = useStore((s) => s.selectedNodeId);
  const selectedNodeKind = useStore((s) => s.selectedNodeKind);
  const selectNode = useStore((s) => s.selectNode);
  const setGraphMode = useStore((s) => s.setGraphMode);
  const seeds = useStore((s) => s.seeds);
  const startDiff = useStore((s) => s.startDiff);
  const startPathfinding = useStore((s) => s.startPathfinding);
  const setGapItems = useStore((s) => s.setGapItems);
  const gapActive = useStore((s) => s.gapActive);
  const toggleGapPanel = useStore((s) => s.toggleGapPanel);

  const analyzeGaps = async () => {
    const items = await loadGapItems();
    setGapItems(items);
    if (!gapActive) toggleGapPanel();
  };

  const compareContext = async () => {
    const compareSeeds = seeds.length > 0 ? seeds : selectedNodeId ? [selectedNodeId] : [];
    if (compareSeeds.length === 0) return;
    const result = await api.brainContext(compareSeeds, 2000, "all");
    startDiff(result, compareSeeds);
  };

  return (
    <div
      data-testid="control-dock"
      className="absolute right-2 top-2 z-50"
    >
      <div className="relative">
        <button
          type="button"
          onClick={() => setOpen((prev) => !prev)}
          title="Settings"
          aria-label="Settings"
          aria-pressed={open}
          className={`flex h-9 w-9 items-center justify-center rounded border transition-colors ${
            open
              ? "border-[var(--color-graph-selection)] bg-[var(--color-surface-alt)] text-[var(--color-graph-selection)]"
              : "border-[var(--color-border)] bg-[var(--color-surface)] text-[var(--color-text-muted)] hover:bg-[var(--color-surface-alt)] hover:text-[var(--color-text)]"
          }`}
        >
          <Settings className="h-4 w-4" />
        </button>

        {open && (
          <div className="absolute right-0 top-11 z-50 w-[320px] max-h-[70vh] overflow-y-auto rounded-md border border-[var(--color-border)] bg-[var(--color-surface)] p-3 text-xs shadow-xl">
            {/* View */}
            <h2 className="mb-2 text-[11px] font-semibold uppercase tracking-wide text-[var(--color-text-muted)]">
              View
            </h2>
            <div className="flex flex-wrap gap-1.5">
              <MenuButton active={viewMode === "graph"} onClick={() => setViewMode("graph")}>
                <Network className="h-3.5 w-3.5" /> Graph
              </MenuButton>
              <MenuButton active={viewMode === "list"} onClick={() => setViewMode("list")}>
                <List className="h-3.5 w-3.5" /> List
              </MenuButton>
              <MenuButton active={viewMode === "matrix"} onClick={() => setViewMode("matrix")}>
                <Grid3X3 className="h-3.5 w-3.5" /> Matrix
              </MenuButton>
              <MenuButton active={minimapVisible} onClick={toggleMinimap}>
                <Map className="h-3.5 w-3.5" /> Minimap
              </MenuButton>
              <MenuButton active={reducedEffects} onClick={toggleReducedEffects}>
                <Sparkles className="h-3.5 w-3.5" /> Reduced effects
              </MenuButton>
              <MenuButton
                active={layoutMode === "zen"}
                onClick={() => setLayoutMode(layoutMode === "zen" ? "panels" : "zen")}
              >
                <Maximize className="h-3.5 w-3.5" /> Focus Map
              </MenuButton>
            </div>

            <hr className="my-3 border-[var(--color-border)]" />

            {/* Group */}
            <h2 className="mb-2 text-[11px] font-semibold uppercase tracking-wide text-[var(--color-text-muted)]">
              Group
            </h2>
            <div className="mb-3 flex flex-wrap gap-1.5">
              <MenuButton active={communityOverlay} onClick={toggleCommunity}>
                <Layers3 className="h-3.5 w-3.5" /> Communities
              </MenuButton>
              <MenuButton active={tagsVisible} onClick={toggleTags}>
                <Tags className="h-3.5 w-3.5" /> Tags
              </MenuButton>
            </div>
            <StyleRules open />

            <hr className="my-3 border-[var(--color-border)]" />

            {/* Filter */}
            <h2 className="mb-2 text-[11px] font-semibold uppercase tracking-wide text-[var(--color-text-muted)]">
              Filter
            </h2>
            <label className="mb-2 block text-[11px] font-medium text-[var(--color-text-muted)]">
              Scope
              <select
                value={scopeFilter}
                onChange={(event) => setScopeFilter(event.target.value as ScopeFilter)}
                className="mt-1 w-full rounded border border-[var(--color-border)] bg-[var(--color-surface-alt)] px-2 py-1 text-xs outline-none"
              >
                <option value="all">All</option>
                <option value="code_only">Code only</option>
                <option value="notes_only">Notes only</option>
              </select>
            </label>
            <NodeFilterBar />

            <hr className="my-3 border-[var(--color-border)]" />

            {/* Analyze */}
            <h2 className="mb-2 text-[11px] font-semibold uppercase tracking-wide text-[var(--color-text-muted)]">
              Analyze
            </h2>
            <div className="mb-3 flex flex-wrap gap-1.5">
              <MenuButton onClick={requestSemanticLayout}>
                <Sparkles className="h-3.5 w-3.5" /> Semantic layout
              </MenuButton>
              <MenuButton onClick={analyzeGaps}>
                <Activity className="h-3.5 w-3.5" /> Gaps
              </MenuButton>
              <MenuButton onClick={compareContext}>
                <GitCompare className="h-3.5 w-3.5" /> Compare
              </MenuButton>
              <MenuButton
                onClick={() => {
                  if (selectedNodeId) startPathfinding(selectedNodeId);
                }}
              >
                <Route className="h-3.5 w-3.5" /> Path
              </MenuButton>
              <MenuButton
                onClick={() => {
                  if (!selectedNodeId) return;
                  selectNode(selectedNodeId, selectedNodeKind);
                  setGraphMode("impact");
                }}
              >
                <Network className="h-3.5 w-3.5" /> Impact
              </MenuButton>
            </div>
            <ForceControls open />

            <hr className="my-3 border-[var(--color-border)]" />

            {/* Export */}
            <h2 className="mb-2 text-[11px] font-semibold uppercase tracking-wide text-[var(--color-text-muted)]">
              Export
            </h2>
            <ExportMenu onClose={() => setOpen(false)} />
          </div>
        )}
      </div>
    </div>
  );
}
