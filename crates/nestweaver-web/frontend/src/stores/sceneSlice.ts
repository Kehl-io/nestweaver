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

function lensKeepsRelationshipResult(lens: ActiveLensState): boolean {
  const label = lens.label.toLowerCase();
  return lens.lens === "search" && (label.startsWith("callers of") || label.startsWith("callees of"));
}

function lensKeepsBacklinkResult(lens: ActiveLensState): boolean {
  return lens.lens === "rationale" && lens.label.toLowerCase().startsWith("backlinks for");
}

export const DEFAULT_IMPACT_DEPTH = 3;
export const DEFAULT_IMPACT_CONFIDENCE = 0.3;

export interface ImpactFilters {
  depth?: number;
  confidence?: number;
}

export function clampImpactDepth(depth: number): number {
  return Math.min(6, Math.max(1, Math.round(depth)));
}

export function clampImpactConfidence(confidence: number): number {
  return Math.min(1, Math.max(0, confidence));
}

export interface SceneSlice {
  activeLens: ActiveLensState;
  representationMode: RepresentationMode;
  sceneMetadata: SceneMetadata | null;
  trustSummary: TrustSummary | null;
  impactDepth: number;
  impactConfidence: number;
  setActiveLens: (lens: ActiveLensState | ActiveLens) => void;
  setRepresentationMode: (mode: RepresentationMode) => void;
  setImpactFilters: (filters: ImpactFilters) => void;
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
  impactDepth: DEFAULT_IMPACT_DEPTH,
  impactConfidence: DEFAULT_IMPACT_CONFIDENCE,

  setActiveLens: (lens) =>
    set((s) => {
      const nextLens =
        typeof lens === "string"
          ? { lens, label: lensLabel(lens), targetUid: null, workspaceId: null }
          : lens;
      const lowerLabel = nextLens.label.toLowerCase();
      const keepDiff = lowerLabel.startsWith("compare");
      const keepGap =
        nextLens.lens === "unsupported" &&
        (lowerLabel.includes("dead code") || lowerLabel.includes("gap"));
      const keepRelationship = lensKeepsRelationshipResult(nextLens);
      const keepBacklinks = lensKeepsBacklinkResult(nextLens);

      s.activeLens = nextLens;
      if (nextLens.lens !== "trace") {
        s.flowTraceRoot = null;
        s.flowTraceNodeUids = [];
        s.flowTraceActive = false;
      }
      if (nextLens.lens !== "path") {
        s.pathfindingActive = false;
        s.pathfindingFrom = null;
        s.pathfindingTo = null;
        s.pathRequestId += 1;
        s.pathResults = [];
        s.pathStatus = "idle";
        s.pathError = null;
        s.selectedPathIndex = 0;
      }
      if (!keepDiff) {
        s.diffActive = false;
        s.diffState.snapshotA = null;
        s.diffState.snapshotB = null;
        s.diffState.seedsA = [];
        s.diffState.seedsB = [];
      }
      if (!keepGap) {
        s.gapItems = [];
        s.gapActive = false;
      }
      if (!keepRelationship) {
        s.relationshipResult = null;
      }
      if (!keepBacklinks) {
        s.backlinkResult = null;
      }
    }),

  setRepresentationMode: (mode) =>
    set((s) => {
      s.representationMode = mode;
      if (mode === "table") {
        s.viewMode = "list";
      } else if (mode === "graph" || mode === "list" || mode === "matrix") {
        s.viewMode = mode;
      }
      // mode === "json": viewMode intentionally keeps the canvas view to
      // return to when the JSON overlay is toggled off (see toggleViewMode)
    }),

  setImpactFilters: (filters) =>
    set((s) => {
      if (filters.depth !== undefined && Number.isFinite(filters.depth)) {
        s.impactDepth = clampImpactDepth(filters.depth);
      }
      if (filters.confidence !== undefined && Number.isFinite(filters.confidence)) {
        s.impactConfidence = clampImpactConfidence(filters.confidence);
      }
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
      s.relationshipResult = null;
      s.backlinkResult = null;
      s.impactDepth = DEFAULT_IMPACT_DEPTH;
      s.impactConfidence = DEFAULT_IMPACT_CONFIDENCE;
    }),
});
