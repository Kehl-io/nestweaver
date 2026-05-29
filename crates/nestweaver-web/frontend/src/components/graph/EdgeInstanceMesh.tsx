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
    const posAttr = new Float32BufferAttribute(buffers.edgePositions, 3);
    const colAttr = new Float32BufferAttribute(buffers.edgeColors, 3);
    geo.setAttribute("position", posAttr);
    geo.setAttribute("color", colAttr);
    geo.setDrawRange(0, buffers.edgeCount * 2);
    geo.computeBoundingSphere();
  }, [buffers]);

  if (buffers.edgeCount === 0) return null;

  return (
    <lineSegments ref={lineRef} frustumCulled={false} renderOrder={-1}>
      <bufferGeometry />
      <lineBasicMaterial vertexColors transparent opacity={0.6} depthTest={false} toneMapped={false} linewidth={1} />
    </lineSegments>
  );
}
