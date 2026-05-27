import { useEffect, useMemo } from "react";
import { useSigma } from "@react-sigma/core";
import { useStore } from "../../stores";
import { desaturate } from "./utils/graphColors";
import { useSemanticZoom } from "./useSemanticZoom";

function hashStringToHue(str: string): number {
  let hash = 0;
  for (let i = 0; i < str.length; i++) {
    hash = str.charCodeAt(i) + ((hash << 5) - hash);
  }
  return Math.abs(hash) % 360;
}

// Read filter state via getState() so reducers always see the current snapshot
// without needing to be re-registered on every filter change.

export function GraphReducers() {
  const sigma = useSigma();
  const selectedNodeId = useStore((s) => s.selectedNodeId);
  const hoveredNodeId = useStore((s) => s.hoveredNodeId);
  const flowTraceActive = useStore((s) => s.flowTraceActive);
  const flowTraceNodeUids = useStore((s) => s.flowTraceNodeUids);
  const pathfindingActive = useStore((s) => s.pathfindingActive);
  const activeStyleRules = useStore((s) => s.activeStyleRules);
  const pathResults = useStore((s) => s.pathResults);
  const selectedPathIndex = useStore((s) => s.selectedPathIndex);
  const theme = useStore((s) => s.theme);
  const nodeTypeFilter = useStore((s) => s.nodeTypeFilter);
  const edgeTypeFilter = useStore((s) => s.edgeTypeFilter);
  const zoomTier = useSemanticZoom();

  // Memoize the neighbor set so the reducer doesn't rebuild it per-node
  const neighborSet = useMemo(() => {
    if (!hoveredNodeId) return null;
    const graph = sigma.getGraph();
    if (!graph.hasNode(hoveredNodeId)) return null;
    const set = new Set(graph.neighbors(hoveredNodeId));
    set.add(hoveredNodeId);
    return set;
  }, [hoveredNodeId, sigma]);

  useEffect(() => {
    const graph = sigma.getGraph();
    const isDark =
      theme === "dark" ||
      (theme === "system" &&
        document.documentElement.classList.contains("dark"));

    // Pre-compute path node set
    let pathNodeSet: Set<string> | null = null;
    if (
      pathfindingActive &&
      pathResults.length > 0 &&
      pathResults[selectedPathIndex]
    ) {
      pathNodeSet = new Set(pathResults[selectedPathIndex].nodes);
    }

    sigma.setSetting(
      "nodeReducer",
      (node: string, data: Record<string, any>) => {
        const res = { ...data };

        // Priority 0: Node type filter — hide filtered-out kinds entirely
        if (data.kind && nodeTypeFilter[data.kind] === false) {
          res.hidden = true;
          return res;
        }

        // Priority 1: Flow trace — highlight path, dim everything else
        if (flowTraceActive && flowTraceNodeUids.length > 0) {
          const traceSet = new Set(flowTraceNodeUids);
          if (traceSet.has(node)) {
            // Traced node: add sequence number badge to label
            const seqIndex = flowTraceNodeUids.indexOf(node);
            if (seqIndex >= 0) {
              res.label = `[${seqIndex + 1}] ${res.label}`;
            }
            res.forceLabel = true;
            res.zIndex = 1;
          } else {
            // Non-traced node: dim
            res.color = isDark ? "#3a3a3a" : "#d0d0d0";
            res.label = "";
            res.zIndex = 0;
          }
        }

        // Priority 2: Pathfinding — dim non-path nodes
        if (pathNodeSet) {
          if (!pathNodeSet.has(node)) {
            res.color = desaturate(data.color || "#999", 0.9);
            res.label = "";
            return res;
          }
          res.forceLabel = true;
          res.highlighted = true;
        }

        // Priority 3: Hover spotlight — dim non-neighbors using fixed ghost colors
        if (neighborSet && !flowTraceActive && !pathNodeSet) {
          if (!neighborSet.has(node)) {
            res.color = isDark ? "#3a3a3a" : "#d0d0d0";
            res.zIndex = 0;
            res.label = "";
          } else if (node === hoveredNodeId) {
            res.zIndex = 1;
            res.highlighted = true;
            res.forceLabel = true;
          } else {
            // Immediate neighbor: keep full color, show label
            res.forceLabel = true;
          }
        }

        // Always: selection highlight
        if (selectedNodeId === node) {
          res.highlighted = true;
          res.forceLabel = true;
        }

        // Semantic zoom label control
        if (zoomTier === "overview") {
          // Truncate labels to 12 chars at overview tier instead of hiding
          if (data.kind !== "Module" && data.kind !== "Class") {
            if (res.label && res.label.length > 12) {
              res.label = res.label.substring(0, 12) + "…";
            }
          }
        } else if (zoomTier === "detail") {
          // Show full signature if available
          if (data.signature) {
            res.label = data.signature;
          }
        }
        // "default" tier: keep normal labels (no change needed)

        // Style rules (applied last so they can override defaults)
        if (activeStyleRules.colorByDir && data.location) {
          const dir = data.location.split("/").slice(0, -1).join("/");
          const hue = hashStringToHue(dir);
          res.color = `hsl(${hue}, 60%, 55%)`;
        }
        if (activeStyleRules.highlightEntryPoints && data.isEntryPoint) {
          res.borderColor = "#eab308";
          res.borderSize = 2;
        }
        if (activeStyleRules.highlightHighPageRank && data.relevance > 0.1) {
          res.borderColor = "#eab308";
          res.borderSize = 2;
        }

        return res;
      },
    );

    sigma.setSetting(
      "edgeReducer",
      (edge: string, data: Record<string, any>) => {
        const res = { ...data };

        // Priority 0: Edge type filter — hide filtered-out edge types entirely
        const edgeType: string = data.edgeType || data.label || "";
        if (edgeType && edgeTypeFilter[edgeType] === false) {
          res.hidden = true;
          return res;
        }

        // Flow trace edge highlight
        if (flowTraceActive && flowTraceNodeUids.length > 0) {
          const traceSet = new Set(flowTraceNodeUids);
          const [source, target] = graph.extremities(edge);
          if (traceSet.has(source) && traceSet.has(target)) {
            // Traced edge: amber, thick
            res.color = "#f59e0b";
            res.size = 3;
          } else {
            // Non-traced edge: hidden
            res.hidden = true;
          }
          return res;
        }

        // Only apply hover edge filtering when no overlay is active
        if (hoveredNodeId && !flowTraceActive && !pathNodeSet) {
          const [source, target] = graph.extremities(edge);
          if (source !== hoveredNodeId && target !== hoveredNodeId) {
            res.hidden = true;
          }
        }

        return res;
      },
    );

    sigma.refresh({ skipIndexation: true });
  }, [
    sigma,
    selectedNodeId,
    hoveredNodeId,
    neighborSet,
    flowTraceActive,
    flowTraceNodeUids,
    pathfindingActive,
    pathResults,
    selectedPathIndex,
    theme,
    nodeTypeFilter,
    edgeTypeFilter,
    zoomTier,
    activeStyleRules,
  ]);

  return null;
}
