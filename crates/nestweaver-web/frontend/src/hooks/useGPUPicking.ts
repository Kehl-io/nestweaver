import { useCallback, useRef } from "react";
import * as THREE from "three";
import type { GraphBuffers } from "./useGraphBridge";

// Encode node index as a unique RGB color
function indexToColor(index: number): [number, number, number] {
  return [
    ((index + 1) & 0xff) / 255,
    (((index + 1) >> 8) & 0xff) / 255,
    (((index + 1) >> 16) & 0xff) / 255,
  ];
}

// Decode pixel color back to node index (-1 if background)
function colorToIndex(r: number, g: number, b: number): number {
  const id = r + (g << 8) + (b << 16);
  return id === 0 ? -1 : id - 1;
}

export interface GPUPickingResult {
  nodeIndex: number;
  nodeUid: string | null;
}

/**
 * Provides a pick() function that identifies which node is at a given screen position.
 * Uses CPU-based distance check against projected positions, which works well for graphs
 * under ~10K nodes. GPU picking via an offscreen render target is an optimization
 * opportunity for larger graphs but is not required at current scale.
 */
export function useGPUPicking(buffers: GraphBuffers) {
  const pickRef = useRef({ buffers });
  pickRef.current.buffers = buffers;

  const pick = useCallback(
    (
      screenX: number,
      screenY: number,
      camera: THREE.Camera,
      size: { width: number; height: number },
    ): GPUPickingResult => {
      const { positions, sizes, nodeCount, indexToUid } = pickRef.current.buffers;

      if (nodeCount === 0) return { nodeIndex: -1, nodeUid: null };

      // Convert screen coords to normalized device coordinates
      const ndcX = (screenX / size.width) * 2 - 1;
      const ndcY = -(screenY / size.height) * 2 + 1;

      let closestIdx = -1;
      let closestDist = Infinity;
      const threshold = 0.05; // NDC units

      // Project each node to NDC and find closest
      const vec3 = new THREE.Vector3();
      for (let i = 0; i < nodeCount; i++) {
        vec3.set(positions[i * 3], positions[i * 3 + 1], positions[i * 3 + 2]);
        vec3.project(camera);

        const dx = vec3.x - ndcX;
        const dy = vec3.y - ndcY;
        const dist = Math.sqrt(dx * dx + dy * dy);
        const nodeThreshold = threshold * Math.max(1, sizes[i] / 6);

        if (dist < nodeThreshold && dist < closestDist) {
          closestDist = dist;
          closestIdx = i;
        }
      }

      return {
        nodeIndex: closestIdx,
        nodeUid: closestIdx >= 0 ? (indexToUid[closestIdx] ?? null) : null,
      };
    },
    [],
  );

  return { pick, indexToColor, colorToIndex };
}
