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

const EDGE_THICKNESS = 1.0;

const vertexShader = /* glsl */ `
  attribute float aStrength;

  varying float v_strength;

  void main() {
    v_strength = aStrength;
    gl_Position = projectionMatrix * modelViewMatrix * instanceMatrix * vec4(position, 1.0);
  }
`;

const fragmentShader = /* glsl */ `
  uniform float u_opacity;
  uniform vec3 u_edgeColor;

  varying float v_strength;

  void main() {
    gl_FragColor = vec4(u_edgeColor, u_opacity * v_strength);
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
        uniforms: {
          u_opacity: { value: 0.5 },
          u_edgeColor: { value: [0.345, 0.357, 0.439] },
        },
        transparent: true,
        depthTest: false,
        depthWrite: false,
        toneMapped: false,
      }),
    [],
  );

  useEffect(() => {
    function updateEdgeColor() {
      const isDark = document.documentElement.classList.contains("dark");
      material.uniforms.u_edgeColor.value = isDark
        ? [0.345, 0.357, 0.439]
        : [0.612, 0.627, 0.690];
    }
    updateEdgeColor();
    const observer = new MutationObserver(updateEdgeColor);
    observer.observe(document.documentElement, { attributes: true, attributeFilter: ["class"] });
    return () => observer.disconnect();
  }, [material]);

  const geometry = useMemo(() => new PlaneGeometry(1, 1), []);

  useEffect(() => {
    const mesh = meshRef.current;
    if (!mesh) return;

    const { edgePositions, edgeCount } = buffers;

    for (let i = 0; i < edgeCount; i++) {
      const sx = edgePositions[i * 6 + 0];
      const sy = edgePositions[i * 6 + 1];
      const tx = edgePositions[i * 6 + 3];
      const ty = edgePositions[i * 6 + 4];

      const mx = (sx + tx) / 2;
      const my = (sy + ty) / 2;

      const dx = tx - sx;
      const dy = ty - sy;
      const length = Math.sqrt(dx * dx + dy * dy);
      const angle = Math.atan2(dy, dx);

      tempObj.position.set(mx, my, -0.1);
      tempObj.rotation.set(0, 0, angle);
      tempObj.scale.set(length, EDGE_THICKNESS, 1);
      tempObj.updateMatrix();
      mesh.setMatrixAt(i, tempObj.matrix);
    }

    mesh.instanceMatrix.needsUpdate = true;

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
