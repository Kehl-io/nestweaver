import { useRef, useEffect } from "react";
import { LineSegments, Float32BufferAttribute } from "three";
import type { GraphBuffers } from "../../hooks/useGraphBridge";

interface Props {
  buffers: GraphBuffers;
}

export function EdgeInstanceMesh({ buffers }: Props) {
  const lineRef = useRef<LineSegments>(null);

  useEffect(() => {
    const line = lineRef.current;
    if (!line) return;

    const geo = line.geometry;
    geo.setAttribute("position", new Float32BufferAttribute(buffers.edgePositions, 3));
    geo.setAttribute("color", new Float32BufferAttribute(buffers.edgeColors, 3));
    geo.computeBoundingSphere();
  }, [buffers]);

  if (buffers.edgeCount === 0) return null;

  return (
    <lineSegments ref={lineRef} frustumCulled={false}>
      <bufferGeometry />
      <lineBasicMaterial vertexColors transparent opacity={0.4} toneMapped={false} />
    </lineSegments>
  );
}
