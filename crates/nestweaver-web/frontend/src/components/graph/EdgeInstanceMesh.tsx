import { useRef, useEffect, useMemo } from "react";
import { InstancedMesh, Object3D, InstancedBufferAttribute } from "three";
import type { GraphBuffers } from "../../hooks/useGraphBridge";

const EDGE_THICKNESS = 0.8; // world units — adjust for visual weight

interface Props {
  buffers: GraphBuffers;
}

export function EdgeInstanceMesh({ buffers }: Props) {
  const meshRef = useRef<InstancedMesh>(null);
  const tempObj = useMemo(() => new Object3D(), []);

  useEffect(() => {
    const mesh = meshRef.current;
    if (!mesh) return;

    const { edgePositions, edgeColors, edgeCount } = buffers;

    // Build instance matrices and colors
    const colors = new Float32Array(edgeCount * 3);

    for (let i = 0; i < edgeCount; i++) {
      const sx = edgePositions[i * 6 + 0];
      const sy = edgePositions[i * 6 + 1];
      const tx = edgePositions[i * 6 + 3];
      const ty = edgePositions[i * 6 + 4];

      // Midpoint
      const mx = (sx + tx) / 2;
      const my = (sy + ty) / 2;

      // Length and angle
      const dx = tx - sx;
      const dy = ty - sy;
      const length = Math.sqrt(dx * dx + dy * dy);
      const angle = Math.atan2(dy, dx);

      tempObj.position.set(mx, my, -0.1); // slightly behind nodes
      tempObj.rotation.set(0, 0, angle);
      tempObj.scale.set(length, EDGE_THICKNESS, 1);
      tempObj.updateMatrix();
      mesh.setMatrixAt(i, tempObj.matrix);

      // Average color of source and target for the edge
      colors[i * 3 + 0] = (edgeColors[i * 6 + 0] + edgeColors[i * 6 + 3]) / 2;
      colors[i * 3 + 1] = (edgeColors[i * 6 + 1] + edgeColors[i * 6 + 4]) / 2;
      colors[i * 3 + 2] = (edgeColors[i * 6 + 2] + edgeColors[i * 6 + 5]) / 2;
    }

    mesh.instanceMatrix.needsUpdate = true;
    mesh.geometry.setAttribute(
      "instanceColor",
      new InstancedBufferAttribute(colors, 3),
    );
  }, [buffers, tempObj]);

  if (buffers.edgeCount === 0) return null;

  return (
    <instancedMesh
      ref={meshRef}
      args={[undefined, undefined, buffers.edgeCount]}
      frustumCulled={false}
      renderOrder={-1}
    >
      {/* Unit quad centered at origin — 1 wide, 1 tall */}
      <planeGeometry args={[1, 1]} />
      <meshBasicMaterial
        vertexColors
        transparent
        opacity={0.45}
        depthTest={false}
        depthWrite={false}
        toneMapped={false}
      />
    </instancedMesh>
  );
}
