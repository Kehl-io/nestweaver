import { useRef, useEffect, useMemo } from "react";
import {
  InstancedMesh,
  Object3D,
  InstancedBufferAttribute,
  ShaderMaterial,
  PlaneGeometry,
} from "three";
import type { GraphBuffers } from "../../hooks/useGraphBridge";
import { useStore } from "../../stores";

const EDGE_THICKNESS = 0.78;

const vertexShader = /* glsl */ `
  attribute vec3 aSourceColor;
  attribute vec3 aTargetColor;
  attribute float aStrength;

  varying vec3 v_sourceColor;
  varying vec3 v_targetColor;
  varying float v_t;
  varying float v_strength;

  void main() {
    v_sourceColor = aSourceColor;
    v_targetColor = aTargetColor;
    v_strength = aStrength;
    // Local X runs from -0.5 (source end) to +0.5 (target end)
    v_t = position.x + 0.5;
    gl_Position = projectionMatrix * modelViewMatrix * instanceMatrix * vec4(position, 1.0);
  }
`;

const fragmentShader = /* glsl */ `
  uniform float u_opacity;

  varying vec3 v_sourceColor;
  varying vec3 v_targetColor;
  varying float v_t;
  varying float v_strength;

  void main() {
    vec3 color = mix(v_sourceColor, v_targetColor, v_t);
    gl_FragColor = vec4(color, u_opacity * v_strength);
  }
`;

interface Props {
  buffers: GraphBuffers;
}

export function EdgeInstanceMesh({ buffers }: Props) {
  const meshRef = useRef<InstancedMesh>(null);
  const tempObj = useMemo(() => new Object3D(), []);
  const hoveredNodeId = useStore((s) => s.hoveredNodeId);
  const selectedNodeId = useStore((s) => s.selectedNodeId);

  const material = useMemo(
    () =>
      new ShaderMaterial({
        vertexShader,
        fragmentShader,
        uniforms: { u_opacity: { value: 0.24 } },
        transparent: true,
        depthTest: false,
        depthWrite: false,
        toneMapped: false,
      }),
    [],
  );

  const geometry = useMemo(() => new PlaneGeometry(1, 1), []);

  useEffect(() => {
    const mesh = meshRef.current;
    if (!mesh) return;

    const { edgePositions, edgeColors, edgeCount } = buffers;

    const sourceColors = new Float32Array(edgeCount * 3);
    const targetColors = new Float32Array(edgeCount * 3);

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

      // Source color (first 3 floats of this edge's color entry)
      sourceColors[i * 3 + 0] = edgeColors[i * 6 + 0];
      sourceColors[i * 3 + 1] = edgeColors[i * 6 + 1];
      sourceColors[i * 3 + 2] = edgeColors[i * 6 + 2];

      // Target color (next 3 floats)
      targetColors[i * 3 + 0] = edgeColors[i * 6 + 3];
      targetColors[i * 3 + 1] = edgeColors[i * 6 + 4];
      targetColors[i * 3 + 2] = edgeColors[i * 6 + 5];
    }

    mesh.instanceMatrix.needsUpdate = true;

    mesh.geometry.setAttribute(
      "aSourceColor",
      new InstancedBufferAttribute(sourceColors, 3),
    );
    mesh.geometry.setAttribute(
      "aTargetColor",
      new InstancedBufferAttribute(targetColors, 3),
    );
    mesh.geometry.setAttribute(
      "aStrength",
      new InstancedBufferAttribute(new Float32Array(edgeCount).fill(1), 1),
    );
  }, [buffers, tempObj]);

  useEffect(() => {
    const mesh = meshRef.current;
    if (!mesh) return;

    const { edgeCount, edgeNodeIndices, uidToIndex } = buffers;
    const strengths = new Float32Array(edgeCount);
    const focusId = hoveredNodeId ?? selectedNodeId;
    const focusIdx = focusId ? uidToIndex.get(focusId) : undefined;

    for (let i = 0; i < edgeCount; i++) {
      const sourceIdx = edgeNodeIndices[i * 2 + 0];
      const targetIdx = edgeNodeIndices[i * 2 + 1];
      const connected =
        focusIdx !== undefined &&
        (sourceIdx === focusIdx || targetIdx === focusIdx);
      strengths[i] =
        focusIdx === undefined
          ? 1
          : connected
            ? 2.35
            : 0.16;
    }

    mesh.geometry.setAttribute(
      "aStrength",
      new InstancedBufferAttribute(strengths, 1),
    );
  }, [buffers, hoveredNodeId, selectedNodeId]);

  if (buffers.edgeCount === 0) return null;

  return (
    <instancedMesh
      ref={meshRef}
      args={[geometry, material, buffers.edgeCount]}
      frustumCulled={false}
      renderOrder={-1}
    />
  );
}
