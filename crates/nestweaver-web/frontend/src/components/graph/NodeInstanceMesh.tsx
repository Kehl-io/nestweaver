import { useRef, useEffect, useMemo } from "react";
import { InstancedMesh, Object3D, InstancedBufferAttribute } from "three";
import type { GraphBuffers } from "../../hooks/useGraphBridge";

interface Props {
  buffers: GraphBuffers;
}

export function NodeInstanceMesh({ buffers }: Props) {
  const meshRef = useRef<InstancedMesh>(null);
  const tempObject = useMemo(() => new Object3D(), []);

  // Update instance matrices when positions/sizes change
  useEffect(() => {
    const mesh = meshRef.current;
    if (!mesh) return;

    const { positions, sizes, nodeCount } = buffers;

    for (let i = 0; i < nodeCount; i++) {
      const x = positions[i * 3];
      const y = positions[i * 3 + 1];
      const z = positions[i * 3 + 2];
      const scale = sizes[i];

      tempObject.position.set(x, y, z);
      tempObject.scale.setScalar(scale);
      tempObject.updateMatrix();
      mesh.setMatrixAt(i, tempObject.matrix);
    }
    mesh.instanceMatrix.needsUpdate = true;
  }, [buffers, tempObject]);

  // Update instance colors
  useEffect(() => {
    const mesh = meshRef.current;
    if (!mesh) return;

    const instanceColors = new Float32Array(buffers.nodeCount * 3);
    for (let i = 0; i < buffers.nodeCount; i++) {
      instanceColors[i * 3] = buffers.colors[i * 3];
      instanceColors[i * 3 + 1] = buffers.colors[i * 3 + 1];
      instanceColors[i * 3 + 2] = buffers.colors[i * 3 + 2];
    }
    mesh.geometry.setAttribute(
      "instanceColor",
      new InstancedBufferAttribute(instanceColors, 3),
    );
  }, [buffers]);

  if (buffers.nodeCount === 0) return null;

  return (
    <instancedMesh
      ref={meshRef}
      args={[undefined, undefined, buffers.nodeCount]}
      frustumCulled={false}
    >
      <circleGeometry args={[1, 32]} />
      <meshBasicMaterial vertexColors toneMapped={false} />
    </instancedMesh>
  );
}
