import { useMemo } from "react";
import { useStore } from "../stores";

export interface GraphBuffers {
  /** [x0, y0, z0, x1, y1, z1, ...] length = nodeCount * 3 */
  positions: Float32Array;
  /** [r0, g0, b0, r1, g1, b1, ...] length = nodeCount * 3 */
  colors: Float32Array;
  /** [s0, s1, ...] length = nodeCount */
  sizes: Float32Array;
  /** [p0, p1, ...] length = nodeCount (per-node phase offset for breathing animation) */
  phases: Float32Array;
  /** [sx0, sy0, sz0, tx0, ty0, tz0, ...] length = edgeCount * 6 */
  edgePositions: Float32Array;
  /** [sr0, sg0, sb0, tr0, tg0, tb0, ...] length = edgeCount * 6 */
  edgeColors: Float32Array;
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
  edgePositions: new Float32Array(0),
  edgeColors: new Float32Array(0),
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

/**
 * Converts the current graphology instance from the store into typed Float32Array
 * buffers suitable for consumption by R3F InstancedMesh components.
 */
export function useGraphBridge(): GraphBuffers {
  const graphInstance = useStore((s) => s.graphInstance);
  const graphVersion = useStore((s) => s.graphVersion);

  return useMemo(() => {
    if (!graphInstance || graphInstance.order === 0) {
      return EMPTY_BUFFERS;
    }

    const graph = graphInstance;
    const nodeCount = graph.order;
    const edgeCount = graph.size;

    // --- Node buffers ---
    const positions = new Float32Array(nodeCount * 3);
    const colors = new Float32Array(nodeCount * 3);
    const sizes = new Float32Array(nodeCount);
    const phases = new Float32Array(nodeCount);
    const uidToIndex = new Map<string, number>();
    const indexToUid: string[] = new Array(nodeCount);

    let ni = 0;
    graph.forEachNode((uid, attrs) => {
      uidToIndex.set(uid, ni);
      indexToUid[ni] = uid;

      // Positions: use x/y from layout; z defaults to 0
      positions[ni * 3 + 0] = typeof attrs.x === "number" ? attrs.x : 0;
      positions[ni * 3 + 1] = typeof attrs.y === "number" ? attrs.y : 0;
      positions[ni * 3 + 2] = typeof attrs.z === "number" ? attrs.z : 0;

      // Colors: parse hex color attribute, fall back to mid-gray
      const [r, g, b] = hexToRgb(typeof attrs.color === "string" ? attrs.color : "");
      colors[ni * 3 + 0] = r;
      colors[ni * 3 + 1] = g;
      colors[ni * 3 + 2] = b;

      // Size: use numeric size attribute, default 1
      sizes[ni] = typeof attrs.size === "number" ? attrs.size : 1;

      // Phase: deterministic per-node float derived from UID
      phases[ni] = hashToPhase(uid);

      ni++;
    });

    // --- Edge buffers ---
    // Each edge contributes 6 floats for positions (source xyz + target xyz)
    // and 6 floats for colors (source rgb + target rgb)
    const edgePositions = new Float32Array(edgeCount * 6);
    const edgeColors = new Float32Array(edgeCount * 6);

    let ei = 0;
    graph.forEachEdge((_edge, _attrs, sourceUid, targetUid) => {
      const si = uidToIndex.get(sourceUid);
      const ti = uidToIndex.get(targetUid);

      if (si !== undefined && ti !== undefined) {
        // Source position
        edgePositions[ei * 6 + 0] = positions[si * 3 + 0];
        edgePositions[ei * 6 + 1] = positions[si * 3 + 1];
        edgePositions[ei * 6 + 2] = positions[si * 3 + 2];
        // Target position
        edgePositions[ei * 6 + 3] = positions[ti * 3 + 0];
        edgePositions[ei * 6 + 4] = positions[ti * 3 + 1];
        edgePositions[ei * 6 + 5] = positions[ti * 3 + 2];

        // Source endpoint color (from source node)
        edgeColors[ei * 6 + 0] = colors[si * 3 + 0];
        edgeColors[ei * 6 + 1] = colors[si * 3 + 1];
        edgeColors[ei * 6 + 2] = colors[si * 3 + 2];
        // Target endpoint color (from target node) — enables directional gradient
        edgeColors[ei * 6 + 3] = colors[ti * 3 + 0];
        edgeColors[ei * 6 + 4] = colors[ti * 3 + 1];
        edgeColors[ei * 6 + 5] = colors[ti * 3 + 2];
      }

      ei++;
    });

    return {
      positions,
      colors,
      sizes,
      phases,
      edgePositions,
      edgeColors,
      uidToIndex,
      indexToUid,
      nodeCount,
      edgeCount,
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [graphInstance, graphVersion]);
}
