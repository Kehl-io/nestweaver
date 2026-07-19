import { useMemo } from "react";
import { useStore } from "../stores";
import { EDGE_COLORS, desaturate, kindColor } from "../components/graph/utils/graphColors";

export interface GraphBuffers {
  /** [x0, y0, z0, x1, y1, z1, ...] length = nodeCount * 3 */
  positions: Float32Array;
  /** [r0, g0, b0, r1, g1, b1, ...] length = nodeCount * 3 */
  colors: Float32Array;
  /** [s0, s1, ...] length = nodeCount */
  sizes: Float32Array;
  /** [p0, p1, ...] length = nodeCount (per-node phase offset for breathing animation) */
  phases: Float32Array;
  /** [i0, i1, ...] length = nodeCount, normalized by relevance and degree */
  importance: Float32Array;
  /** [s0, s1, ...] length = nodeCount, 1 when the node is an active seed */
  seedMarkers: Float32Array;
  /** [b0, b1, ...] length = nodeCount, normalized bridge/hub strength */
  bridgeStrengths: Float32Array;
  /** [sx0, sy0, sz0, tx0, ty0, tz0, ...] length = edgeCount * 6 */
  edgePositions: Float32Array;
  /** [sr0, sg0, sb0, tr0, tg0, tb0, ...] length = edgeCount * 6 */
  edgeColors: Float32Array;
  /** [sourceIndex0, targetIndex0, ...] length = edgeCount * 2 */
  edgeNodeIndices: Int32Array;
  /** [t0, t1, ...] length = edgeCount, 1 = intra-galaxy tinted edge */
  edgeTints: Float32Array;
  uidToIndex: Map<string, number>;
  indexToUid: string[];
  nodeCount: number;
  edgeCount: number;
}

export const EMPTY_BUFFERS: GraphBuffers = {
  positions: new Float32Array(0),
  colors: new Float32Array(0),
  sizes: new Float32Array(0),
  phases: new Float32Array(0),
  importance: new Float32Array(0),
  seedMarkers: new Float32Array(0),
  bridgeStrengths: new Float32Array(0),
  edgePositions: new Float32Array(0),
  edgeColors: new Float32Array(0),
  edgeNodeIndices: new Int32Array(0),
  edgeTints: new Float32Array(0),
  uidToIndex: new Map(),
  indexToUid: [],
  nodeCount: 0,
  edgeCount: 0,
};

/**
 * Parse a hex color string (#rrggbb or #rgb) into [r, g, b] floats in [0, 1].
 * Falls back to [0.5, 0.5, 0.5] if the string is not a recognized hex color.
 */
function hexToRgb(hex: string): [number, number, number] {
  if (!hex || hex[0] !== "#") return [0.5, 0.5, 0.5];
  const clean = hex.slice(1);
  if (clean.length === 3) {
    const r = parseInt(clean[0] + clean[0], 16) / 255;
    const g = parseInt(clean[1] + clean[1], 16) / 255;
    const b = parseInt(clean[2] + clean[2], 16) / 255;
    return [r, g, b];
  }
  if (clean.length === 6) {
    const r = parseInt(clean.slice(0, 2), 16) / 255;
    const g = parseInt(clean.slice(2, 4), 16) / 255;
    const b = parseInt(clean.slice(4, 6), 16) / 255;
    return [r, g, b];
  }
  return [0.5, 0.5, 0.5];
}

function edgeColorForType(type: unknown): [number, number, number] | null {
  if (typeof type !== "string") return null;
  // "overview" (intra-galaxy) edges intentionally return null so the
  // node-color fallback tints them with their galaxy's hue (glowing web)
  if (type === "overview") return null;
  const color = EDGE_COLORS[type];
  return color ? hexToRgb(color) : null;
}

/**
 * Simple hash of a string into a float in [0, 1) for deterministic phase offsets.
 */
function hashToPhase(uid: string): number {
  let h = 0x811c9dc5;
  for (let i = 0; i < uid.length; i++) {
    h ^= uid.charCodeAt(i);
    h = (h * 0x01000193) >>> 0;
  }
  return (h >>> 0) / 0xffffffff;
}

function numericMetric(attrs: Record<string, unknown>, names: string[]): number {
  for (const name of names) {
    const value = attrs[name];
    if (typeof value === "number" && Number.isFinite(value)) return value;
  }
  return 0;
}

/**
 * Hash a string to a hue value in [0, 360).
 */
function hashStringToHue(s: string): number {
  let h = 0x811c9dc5;
  for (let i = 0; i < s.length; i++) {
    h ^= s.charCodeAt(i);
    h = (h * 0x01000193) >>> 0;
  }
  return ((h >>> 0) % 360);
}

/**
 * Convert HSL (h in [0,1], s in [0,1], l in [0,1]) to RGB floats in [0,1].
 */
function hslToRgb(h: number, s: number, l: number): [number, number, number] {
  const c = (1 - Math.abs(2 * l - 1)) * s;
  const x = c * (1 - Math.abs(((h * 6) % 2) - 1));
  const m = l - c / 2;
  let r = 0, g = 0, b = 0;
  const hd = h * 6;
  if (hd < 1) { r = c; g = x; b = 0; }
  else if (hd < 2) { r = x; g = c; b = 0; }
  else if (hd < 3) { r = 0; g = c; b = x; }
  else if (hd < 4) { r = 0; g = x; b = c; }
  else if (hd < 5) { r = x; g = 0; b = c; }
  else { r = c; g = 0; b = x; }
  return [r + m, g + m, b + m];
}

/**
 * Converts the current graphology instance from the store into typed Float32Array
 * buffers suitable for consumption by R3F InstancedMesh components.
 */
export function useGraphBridge(): GraphBuffers {
  const graphInstance = useStore((s) => s.graphInstance);
  const graphVersion = useStore((s) => s.graphVersion);
  const activeStyleRules = useStore((s) => s.activeStyleRules);
  const seeds = useStore((s) => s.seeds);
  const theme = useStore((s) => s.theme);

  return useMemo(() => {
    // Graphology is mutated in place. Consume the store's version token so
    // buffer generation reruns even when the graph reference is unchanged.
    void graphVersion;
    if (!graphInstance || graphInstance.order === 0) {
      return EMPTY_BUFFERS;
    }

    const graph = graphInstance;
    const isDark =
      theme === "dark" ||
      (theme === "system" &&
        typeof window !== "undefined" &&
        window.matchMedia("(prefers-color-scheme: dark)").matches);
    const nodeCount = graph.order;
    const edgeCount = graph.size;

    // --- Node buffers ---
    const positions = new Float32Array(nodeCount * 3);
    const colors = new Float32Array(nodeCount * 3);
    const sizes = new Float32Array(nodeCount);
    const phases = new Float32Array(nodeCount);
    const importance = new Float32Array(nodeCount);
    const seedMarkers = new Float32Array(nodeCount);
    const bridgeStrengths = new Float32Array(nodeCount);
    const uidToIndex = new Map<string, number>();
    const indexToUid: string[] = new Array(nodeCount);
    const seedSet = new Set(seeds);

    let maxRelevance = 0;
    let maxDegree = 0;
    graph.forEachNode((uid, attrs) => {
      const relevance = numericMetric(attrs, [
        "relevance",
        "pagerank",
        "pagerank_score",
      ]);
      maxRelevance = Math.max(maxRelevance, relevance);
      maxDegree = Math.max(maxDegree, graph.degree(uid));
    });

    let ni = 0;
    graph.forEachNode((uid, attrs) => {
      uidToIndex.set(uid, ni);
      indexToUid[ni] = uid;

      // Positions: use x/y from layout; z defaults to 0
      positions[ni * 3 + 0] = typeof attrs.x === "number" ? attrs.x : 0;
      positions[ni * 3 + 1] = typeof attrs.y === "number" ? attrs.y : 0;
      positions[ni * 3 + 2] = typeof attrs.z === "number" ? attrs.z : 0;

      // Colors: derive from paletteKind when present (stays correct across
      // theme flips), else parse the baked hex color, fall back to mid-gray
      let colorHex = typeof attrs.color === "string" ? attrs.color : "";
      if (typeof attrs.paletteKind === "string") {
        colorHex = kindColor(attrs.paletteKind, isDark);
        const fade = typeof attrs.colorDesaturate === "number" ? attrs.colorDesaturate : 0;
        if (fade > 0) colorHex = desaturate(colorHex, fade);
      }
      let [r, g, b] = hexToRgb(colorHex);

      // Style rule: color by directory
      if (activeStyleRules.colorByDir && typeof attrs.location === "string") {
        const dir = attrs.location.split("/").slice(0, -1).join("/");
        const hue = hashStringToHue(dir);
        [r, g, b] = hslToRgb(hue / 360, 0.6, 0.55);
      }

      colors[ni * 3 + 0] = r;
      colors[ni * 3 + 1] = g;
      colors[ni * 3 + 2] = b;

      // Size: use numeric size attribute, default 1
      let nodeSize = typeof attrs.size === "number" ? attrs.size : 1;

      // Style rule: boost size for entry points
      if (activeStyleRules.highlightEntryPoints && attrs.isEntryPoint === true) {
        nodeSize *= 2.0;
      }

      // Style rule: boost size for high-PageRank nodes
      if (activeStyleRules.highlightHighPageRank && typeof attrs.relevance === "number" && attrs.relevance > 0.1) {
        nodeSize *= 1.8;
      }

      sizes[ni] = nodeSize;

      // Phase: deterministic per-node float derived from UID
      phases[ni] = hashToPhase(uid);

      const relevance = numericMetric(attrs, [
        "relevance",
        "pagerank",
        "pagerank_score",
      ]);
      const degree = graph.degree(uid);
      const relevanceScore = maxRelevance > 0 ? relevance / maxRelevance : 0;
      const degreeScore = maxDegree > 0 ? degree / maxDegree : 0;
      importance[ni] = Math.min(1, Math.max(relevanceScore, degreeScore * 0.7));
      seedMarkers[ni] = attrs.isSeed === true || seedSet.has(uid) ? 1 : 0;
      // Real betweenness from the backend when present (top-12 per scene,
      // normalized); degree heuristic only as a fallback for older payloads
      const bridgeScore = numericMetric(attrs, ["bridgeScore"]);
      bridgeStrengths[ni] =
        bridgeScore > 0
          ? Math.min(1, bridgeScore)
          : degree >= 3 && degreeScore >= 0.45 && seedMarkers[ni] === 0
            ? Math.min(1, degreeScore)
            : 0;

      ni++;
    });

    // --- Edge buffers ---
    // Each edge contributes 6 floats for positions (source xyz + target xyz)
    // and 6 floats for colors (source rgb + target rgb)
    const edgePositions = new Float32Array(edgeCount * 6);
    const edgeColors = new Float32Array(edgeCount * 6);
    const edgeNodeIndices = new Int32Array(edgeCount * 2);
    // 1 = intra-galaxy edge (tinted glowing web), 0 = cross-cutting (neutral)
    const edgeTints = new Float32Array(edgeCount);

    let ei = 0;
    graph.forEachEdge((_edge, _attrs, sourceUid, targetUid) => {
      const si = uidToIndex.get(sourceUid);
      const ti = uidToIndex.get(targetUid);

      if (si !== undefined && ti !== undefined) {
        edgeNodeIndices[ei * 2 + 0] = si;
        edgeNodeIndices[ei * 2 + 1] = ti;

        // Source position
        edgePositions[ei * 6 + 0] = positions[si * 3 + 0];
        edgePositions[ei * 6 + 1] = positions[si * 3 + 1];
        edgePositions[ei * 6 + 2] = positions[si * 3 + 2];
        // Target position
        edgePositions[ei * 6 + 3] = positions[ti * 3 + 0];
        edgePositions[ei * 6 + 4] = positions[ti * 3 + 1];
        edgePositions[ei * 6 + 5] = positions[ti * 3 + 2];

        const edgeColor = edgeColorForType(_attrs.type);
        const sourceColor = edgeColor ?? [
          colors[si * 3 + 0],
          colors[si * 3 + 1],
          colors[si * 3 + 2],
        ];
        const targetColor = edgeColor ?? [
          colors[ti * 3 + 0],
          colors[ti * 3 + 1],
          colors[ti * 3 + 2],
        ];

        edgeColors[ei * 6 + 0] = sourceColor[0];
        edgeColors[ei * 6 + 1] = sourceColor[1];
        edgeColors[ei * 6 + 2] = sourceColor[2];
        edgeColors[ei * 6 + 3] = targetColor[0];
        edgeColors[ei * 6 + 4] = targetColor[1];
        edgeColors[ei * 6 + 5] = targetColor[2];

        edgeTints[ei] = _attrs.type === "overview" ? 1 : 0;
      }

      ei++;
    });

    return {
      positions,
      colors,
      sizes,
      phases,
      importance,
      seedMarkers,
      bridgeStrengths,
      edgePositions,
      edgeColors,
      edgeNodeIndices,
      edgeTints,
      uidToIndex,
      indexToUid,
      nodeCount,
      edgeCount,
    };
  }, [graphInstance, graphVersion, activeStyleRules, seeds, theme]);
}
