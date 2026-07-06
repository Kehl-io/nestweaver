import { useEffect, useRef } from "react";
import { useStore } from "../stores";
import type { GraphMode } from "../api/types";
import type { ActiveLens, RepresentationMode } from "../api/p1Types";

const graphModes: GraphMode[] = [
  "overview",
  "context",
  "impact",
  "repos",
  "features",
  "local",
];
const activeLenses: ActiveLens[] = [
  "overview",
  "context",
  "search",
  "impact",
  "trace",
  "path",
  "rationale",
  "freshness",
  "unsupported",
];
const representationModes: RepresentationMode[] = [
  "graph",
  "list",
  "table",
  "matrix",
  "json",
];

function validGraphMode(value: string | null): GraphMode | null {
  return value && graphModes.includes(value as GraphMode)
    ? (value as GraphMode)
    : null;
}

function validLens(value: string | null): ActiveLens | null {
  return value && activeLenses.includes(value as ActiveLens)
    ? (value as ActiveLens)
    : null;
}

function validRepresentation(value: string | null): RepresentationMode | null {
  return value && representationModes.includes(value as RepresentationMode)
    ? (value as RepresentationMode)
    : null;
}

function graphModeForLens(lens: ActiveLens): GraphMode | null {
  if (lens === "overview" || lens === "context" || lens === "impact") {
    return lens;
  }
  return null;
}

export function useDeepLink() {
  const seeds = useStore((s) => s.seeds);
  const graphMode = useStore((s) => s.graphMode);
  const activeWorkspaceId = useStore((s) => s.activeWorkspaceId);
  const selectedNodeId = useStore((s) => s.selectedNodeId);
  const selectedNodeKind = useStore((s) => s.selectedNodeKind);
  const activeLens = useStore((s) => s.activeLens);
  const representationMode = useStore((s) => s.representationMode);
  const setSeeds = useStore((s) => s.setSeeds);
  const setGraphMode = useStore((s) => s.setGraphMode);
  const setActiveWorkspaceId = useStore((s) => s.setActiveWorkspaceId);
  const selectNode = useStore((s) => s.selectNode);
  const setActiveLens = useStore((s) => s.setActiveLens);
  const setRepresentationMode = useStore((s) => s.setRepresentationMode);
  const initializedRef = useRef(false);
  const skipNextWriteRef = useRef(false);

  // On mount: read URL params and apply to store
  useEffect(() => {
    if (initializedRef.current) return;
    initializedRef.current = true;

    const params = new URLSearchParams(window.location.search);
    skipNextWriteRef.current = params.toString().length > 0;
    const seedParam = params.get("seeds");
    const modeParam = params.get("mode");
    const workspaceParam = params.get("workspace");
    const nodeParam = params.get("node");
    const kindParam = params.get("kind");
    const lensParam = validLens(params.get("lens"));
    const representationParam = validRepresentation(
      params.get("representation") ?? params.get("view"),
    );

    if (seedParam) {
      setSeeds(seedParam.split(",").filter(Boolean));
    }
    if (workspaceParam) {
      setActiveWorkspaceId(workspaceParam);
    }
    if (nodeParam) {
      selectNode(nodeParam, kindParam);
    }
    if (representationParam) {
      setRepresentationMode(representationParam);
    }
    if (lensParam) {
      setActiveLens({
        lens: lensParam,
        label: lensParam.charAt(0).toUpperCase() + lensParam.slice(1),
        targetUid: nodeParam,
        workspaceId: workspaceParam,
      });
      const modeFromLens = graphModeForLens(lensParam);
      if (modeFromLens) {
        setGraphMode(modeFromLens);
      }
    }
    const validMode = validGraphMode(modeParam);
    if (validMode) {
      setGraphMode(validMode);
    }
  }, [
    selectNode,
    setActiveLens,
    setActiveWorkspaceId,
    setGraphMode,
    setRepresentationMode,
    setSeeds,
  ]);

  // On state change: update URL
  useEffect(() => {
    if (!initializedRef.current) return;
    if (skipNextWriteRef.current) {
      skipNextWriteRef.current = false;
      return;
    }

    const params = new URLSearchParams();
    if (seeds.length > 0) params.set("seeds", seeds.join(","));
    if (graphMode !== "overview") params.set("mode", graphMode);
    if (activeWorkspaceId !== "all") params.set("workspace", activeWorkspaceId);
    if (selectedNodeId) params.set("node", selectedNodeId);
    if (selectedNodeKind) params.set("kind", selectedNodeKind);
    if (activeLens.lens !== "overview") params.set("lens", activeLens.lens);
    if (representationMode !== "graph") {
      params.set("representation", representationMode);
    }

    const url = params.toString()
      ? `${window.location.pathname}?${params}`
      : window.location.pathname;

    window.history.replaceState(null, "", url);
  }, [
    activeLens,
    activeWorkspaceId,
    graphMode,
    representationMode,
    seeds,
    selectedNodeId,
    selectedNodeKind,
  ]);
}
