import type { StateCreator } from "zustand";
import type { GraphMode, ScopeFilter } from "../api/types";
import type { StoreState } from "./index";

export interface GraphSlice {
  selectedNodeId: string | null;
  selectedNodeKind: string | null;
  hoveredNodeId: string | null;
  graphMode: GraphMode;
  seeds: string[];
  scopeFilter: ScopeFilter;
  scopeRepoUid: string | null;
  scopeVaultUid: string | null;
  communityOverlay: boolean;
  tagsVisible: boolean;
  minimapVisible: boolean;
  selectNode: (id: string | null, kind?: string | null) => void;
  hoverNode: (id: string | null) => void;
  setGraphMode: (mode: GraphMode) => void;
  setSeeds: (seeds: string[]) => void;
  setScopeFilter: (filter: ScopeFilter) => void;
  setScopeRepo: (uid: string | null) => void;
  setScopeVault: (uid: string | null) => void;
  toggleCommunityOverlay: () => void;
  toggleTags: () => void;
  toggleMinimap: () => void;
  semanticLayoutRequested: boolean;
  requestSemanticLayout: () => void;
  clearSemanticLayoutRequest: () => void;
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
  graphMode: "context",
  seeds: [],
  scopeFilter: "all",
  scopeRepoUid: null,
  scopeVaultUid: null,
  communityOverlay: false,
  tagsVisible: true,
  minimapVisible: true,

  selectNode: (id, kind) =>
    set((s) => {
      s.selectedNodeId = id;
      s.selectedNodeKind = kind ?? null;
    }),

  hoverNode: (id) =>
    set((s) => {
      s.hoveredNodeId = id;
    }),

  setGraphMode: (mode) =>
    set((s) => {
      s.graphMode = mode;
    }),

  setSeeds: (seeds) =>
    set((s) => {
      s.seeds = seeds;
    }),

  setScopeFilter: (filter) =>
    set((s) => {
      s.scopeFilter = filter;
    }),

  setScopeRepo: (uid) =>
    set((s) => {
      s.scopeRepoUid = uid;
    }),

  setScopeVault: (uid) =>
    set((s) => {
      s.scopeVaultUid = uid;
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
});
