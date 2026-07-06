import type { StateCreator } from "zustand";
import type {
  ActiveLens,
  ActiveLensState,
  RepresentationMode,
  SceneMetadata,
  TrustSummary,
} from "../api/p1Types";
import type { StoreState } from "./index";

const defaultLens: ActiveLensState = {
  lens: "overview",
  label: "Overview",
  targetUid: null,
  workspaceId: null,
};

function lensLabel(lens: ActiveLens): string {
  switch (lens) {
    case "context":
      return "Context";
    case "search":
      return "Search";
    case "impact":
      return "Impact";
    case "trace":
      return "Trace";
    case "path":
      return "Path";
    case "rationale":
      return "Rationale";
    case "freshness":
      return "Freshness";
    case "unsupported":
      return "Unsupported";
    case "overview":
      return "Overview";
  }
}

function trustSummaryFromMetadata(metadata: SceneMetadata): TrustSummary {
  return {
    dataScope: metadata.trust.data_scope,
    freshness: metadata.trust.freshness,
    federation: metadata.trust.federation,
    result: metadata.trust.result,
    partial: metadata.trust.partial,
    unsupported: metadata.trust.unsupported,
    message: metadata.trust.message,
  };
}

export interface SceneSlice {
  activeLens: ActiveLensState;
  representationMode: RepresentationMode;
  sceneMetadata: SceneMetadata | null;
  trustSummary: TrustSummary | null;
  setActiveLens: (lens: ActiveLensState | ActiveLens) => void;
  setRepresentationMode: (mode: RepresentationMode) => void;
  setSceneMetadata: (metadata: SceneMetadata | null) => void;
  setTrustSummary: (summary: TrustSummary | null) => void;
  clearSceneMetadata: () => void;
  clearSceneState: () => void;
}

export const createSceneSlice: StateCreator<
  StoreState,
  [["zustand/immer", never]],
  [],
  SceneSlice
> = (set) => ({
  activeLens: { ...defaultLens },
  representationMode: "graph",
  sceneMetadata: null,
  trustSummary: null,

  setActiveLens: (lens) =>
    set((s) => {
      s.activeLens =
        typeof lens === "string"
          ? { lens, label: lensLabel(lens), targetUid: null, workspaceId: null }
          : lens;
    }),

  setRepresentationMode: (mode) =>
    set((s) => {
      s.representationMode = mode;
      s.viewMode = mode === "table" ? "list" : mode;
    }),

  setSceneMetadata: (metadata) =>
    set((s) => {
      s.sceneMetadata = metadata;
      s.trustSummary = metadata ? trustSummaryFromMetadata(metadata) : null;
    }),

  setTrustSummary: (summary) =>
    set((s) => {
      s.trustSummary = summary;
    }),

  clearSceneMetadata: () =>
    set((s) => {
      s.sceneMetadata = null;
      s.trustSummary = null;
    }),

  clearSceneState: () =>
    set((s) => {
      s.activeLens = { ...defaultLens };
      s.representationMode = "graph";
      s.viewMode = "graph";
      s.sceneMetadata = null;
      s.trustSummary = null;
    }),
});
