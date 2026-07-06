import type { StateCreator } from "zustand";
import type { GraphMode, ScopeFilter } from "../api/types";
import type { StoreState } from "./index";

export type DetailFocus = "summary" | "source" | "related" | "analysis";
export type ViewMode = "graph" | "list" | "matrix";

export interface GraphSlice {
  selectedNodeId: string | null;
  selectedNodeKind: string | null;
  hoveredNodeId: string | null;
  previewNodeId: string | null;
  previewExpanded: boolean;
  openPreview: (id: string, kind?: string | null) => void;
  closePreview: () => void;
  togglePreviewExpanded: () => void;
  contextMenu: { x: number; y: number; nodeId: string } | null;
  openContextMenu: (x: number, y: number, nodeId: string) => void;
  closeContextMenu: () => void;
  graphMode: GraphMode;
  seeds: string[];
  scopeFilter: ScopeFilter;
  communityOverlay: boolean;
  tagsVisible: boolean;
  minimapVisible: boolean;
  nodeTypeFilter: Record<string, boolean>;
  edgeTypeFilter: Record<string, boolean>;
  forceParams: { repulsion: number; gravity: number; settling: number };
  layoutMode: "panels" | "zen";
  activeStyleRules: Record<string, boolean>;
  reducedEffects: boolean;
  reducedEffectsUserSet: boolean;
  setReducedEffects: (reduced: boolean) => void;
  seedReducedEffectsFromSystem: (reduced: boolean) => void;
  toggleReducedEffects: () => void;
  viewMode: ViewMode;
  detailFocus: DetailFocus;
  toggleViewMode: () => void;
  setViewMode: (mode: ViewMode) => void;
  setDetailFocus: (focus: DetailFocus) => void;
  cameraZoom: number;
  setCameraZoom: (zoom: number) => void;
  selectNode: (id: string | null, kind?: string | null) => void;
  exploreNode: (id: string, kind?: string | null) => void;
  hoverNode: (id: string | null) => void;
  setGraphMode: (mode: GraphMode) => void;
  setSeeds: (seeds: string[]) => void;
  addSeed: (uid: string) => void;
  setScopeFilter: (filter: ScopeFilter) => void;
  toggleCommunityOverlay: () => void;
  toggleTags: () => void;
  toggleMinimap: () => void;
  semanticLayoutRequested: boolean;
  requestSemanticLayout: () => void;
  clearSemanticLayoutRequest: () => void;
  setNodeTypeFilter: (kind: string, visible: boolean) => void;
  setAllNodeTypes: (visible: boolean) => void;
  setEdgeTypeFilter: (type: string, visible: boolean) => void;
  setForceParams: (params: Partial<{ repulsion: number; gravity: number; settling: number }>) => void;
  setLayoutMode: (mode: "panels" | "zen") => void;
  toggleStyleRule: (rule: string) => void;
}

export const createGraphSlice: StateCreator<
  StoreState,
  [["zustand/immer", never]],
  [],
  GraphSlice
> = (set) => ({
  selectedNodeId: null,
  selectedNodeKind: null,
  hoveredNodeId: null,
  previewNodeId: null,
  previewExpanded: false,
  contextMenu: null,
  graphMode: "overview",
  seeds: [],
  scopeFilter: "all",
  communityOverlay: false,
  tagsVisible: true,
  minimapVisible: true,
  nodeTypeFilter: {
    Function: true,
    Class: true,
    Method: true,
    Interface: true,
    Trait: true,
    Enum: true,
    Module: true,
    Note: true,
    Tag: true,
  },
  edgeTypeFilter: {
    calls: true,
    imports: true,
    extends: true,
    implements: true,
    includes: true,
  },
  forceParams: { repulsion: 2, gravity: 1, settling: 10 },
  layoutMode: "zen" as const,
  activeStyleRules: {
    colorByDir: false,
    sizeByCallers: false,
    highlightEntryPoints: false,
    highlightHighPageRank: false,
  },
  reducedEffects: false,
  reducedEffectsUserSet: false,
  viewMode: "graph" as const,
  detailFocus: "summary" as const,
  cameraZoom: 1,

  setReducedEffects: (reduced) =>
    set((s) => {
      s.reducedEffects = reduced;
    }),

  seedReducedEffectsFromSystem: (reduced) =>
    set((s) => {
      if (!s.reducedEffectsUserSet) {
        s.reducedEffects = reduced;
      }
    }),

  toggleReducedEffects: () =>
    set((s) => {
      s.reducedEffectsUserSet = true;
      s.reducedEffects = !s.reducedEffects;
    }),

  toggleViewMode: () =>
    set((s) => {
      s.viewMode =
        s.viewMode === "graph"
          ? "list"
          : s.viewMode === "list"
            ? "matrix"
            : "graph";
      s.representationMode = s.viewMode;
    }),

  setViewMode: (mode) =>
    set((s) => {
      s.viewMode = mode;
      s.representationMode = mode;
    }),

  setDetailFocus: (focus) =>
    set((s) => {
      s.detailFocus = focus;
    }),

  setCameraZoom: (zoom) =>
    set((s) => {
      s.cameraZoom = zoom;
    }),

  selectNode: (id, kind) =>
    set((s) => {
      s.selectedNodeId = id;
      s.selectedNodeKind = kind ?? null;
      s.detailFocus = "summary";
      if (id === null) {
        s.previewNodeId = null;
        s.previewExpanded = false;
      }
    }),

  exploreNode: (id, kind) =>
    set((s) => {
      s.selectedNodeId = id;
      s.selectedNodeKind = kind ?? null;
      s.seeds = [id];
      s.graphMode = "context";
      s.detailFocus = "summary";
    }),

  hoverNode: (id) =>
    set((s) => {
      s.hoveredNodeId = id;
    }),

  openPreview: (id, kind) =>
    set((s) => {
      s.previewNodeId = id;
      s.previewExpanded = false;
      s.selectedNodeId = id;
      s.selectedNodeKind = kind ?? null;
    }),

  closePreview: () =>
    set((s) => {
      s.previewNodeId = null;
      s.previewExpanded = false;
    }),

  togglePreviewExpanded: () =>
    set((s) => {
      s.previewExpanded = !s.previewExpanded;
    }),

  openContextMenu: (x, y, nodeId) =>
    set((s) => {
      s.contextMenu = { x, y, nodeId };
    }),

  closeContextMenu: () =>
    set((s) => {
      s.contextMenu = null;
    }),

  setGraphMode: (mode) =>
    set((s) => {
      s.graphMode = mode;
    }),

  setSeeds: (seeds) =>
    set((s) => {
      s.seeds = seeds;
    }),

  addSeed: (uid) =>
    set((s) => {
      if (!s.seeds.includes(uid)) s.seeds.push(uid);
      s.graphMode = "context";
    }),

  setScopeFilter: (filter) =>
    set((s) => {
      s.scopeFilter = filter;
    }),

  toggleCommunityOverlay: () =>
    set((s) => {
      s.communityOverlay = !s.communityOverlay;
    }),

  toggleTags: () =>
    set((s) => {
      s.tagsVisible = !s.tagsVisible;
    }),

  toggleMinimap: () =>
    set((s) => {
      s.minimapVisible = !s.minimapVisible;
    }),

  semanticLayoutRequested: false,
  requestSemanticLayout: () =>
    set((s) => {
      s.semanticLayoutRequested = true;
    }),

  clearSemanticLayoutRequest: () =>
    set((s) => {
      s.semanticLayoutRequested = false;
    }),

  setNodeTypeFilter: (kind, visible) =>
    set((s) => {
      s.nodeTypeFilter[kind] = visible;
    }),

  setAllNodeTypes: (visible) =>
    set((s) => {
      Object.keys(s.nodeTypeFilter).forEach((kind) => {
        s.nodeTypeFilter[kind] = visible;
      });
    }),

  setEdgeTypeFilter: (type, visible) =>
    set((s) => {
      s.edgeTypeFilter[type] = visible;
    }),

  setForceParams: (params) =>
    set((s) => {
      Object.assign(s.forceParams, params);
    }),

  setLayoutMode: (mode) =>
    set((s) => {
      s.layoutMode = mode;
    }),

  toggleStyleRule: (rule) =>
    set((s) => {
      s.activeStyleRules[rule] = !s.activeStyleRules[rule];
    }),
});
