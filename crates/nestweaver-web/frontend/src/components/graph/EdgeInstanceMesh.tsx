import { useRef, useEffect, useMemo, useState } from "react";
import {
  AdditiveBlending,
  InstancedMesh,
  NormalBlending,
  Object3D,
  InstancedBufferAttribute,
  ShaderMaterial,
  PlaneGeometry,
} from "three";
import type { GraphBuffers } from "../../hooks/useGraphBridge";
import { useStore } from "../../stores";

const EDGE_THICKNESS = 1.2;
// Additive tinting is capped to bound fill-rate on dense scenes (blended
// overdraw is the first-order GPU cost — cosmos.gl exposes the same escape
// hatch via its linkBlending option)
const ADDITIVE_EDGE_LIMIT = 5000;

const vertexShader = /* glsl */ `
  attribute float aStrength;
  attribute vec3 aColorA;
  attribute vec3 aColorB;
  attribute float aTint;

  varying float v_strength;
  varying vec3 v_colorA;
  varying vec3 v_colorB;
  varying float v_tint;
  varying vec2 v_uv;

  void main() {
    v_strength = aStrength;
    v_colorA = aColorA;
    v_colorB = aColorB;
    v_tint = aTint;
    v_uv = uv;
    gl_Position = projectionMatrix * modelViewMatrix * instanceMatrix * vec4(position, 1.0);
  }
`;

const fragmentShader = /* glsl */ `
  uniform float u_opacity;
  uniform vec3 u_edgeColor;
  uniform float u_tintAmp;

  varying float v_strength;
  varying vec3 v_colorA;
  varying vec3 v_colorB;
  varying float v_tint;
  varying vec2 v_uv;

  void main() {
    // Soft cross-section: the quad reads as a glowing line, not a bar
    float falloff = 1.0 - smoothstep(0.12, 0.5, abs(v_uv.y - 0.5));

    // Intra-galaxy edges take their galaxy hue (gradient source -> target);
    // cross-cutting edges stay neutral so structure boundaries read clearly
    vec3 galaxyColor = mix(v_colorA, v_colorB, v_uv.x);
    float tint = v_tint * u_tintAmp;
    vec3 color = mix(u_edgeColor, galaxyColor, tint);

    // Tinted web sits in the low additive band (0.06-0.25 by design);
    // neutral edges keep the previous alpha profile
    float baseAlpha = mix(u_opacity, 0.14, tint);
    gl_FragColor = vec4(color, baseAlpha * v_strength * falloff);
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
  const [isDark, setIsDark] = useState(() =>
    typeof document !== "undefined"
      ? document.documentElement.classList.contains("dark")
      : true,
  );

  const additive = isDark && buffers.edgeCount <= ADDITIVE_EDGE_LIMIT;

  const material = useMemo(
    () =>
      new ShaderMaterial({
        vertexShader,
        fragmentShader,
        uniforms: {
          u_opacity: { value: 0.5 },
          u_edgeColor: { value: [0.345, 0.357, 0.439] },
          u_tintAmp: { value: 0 },
        },
        transparent: true,
        depthTest: false,
        depthWrite: false,
        toneMapped: false,
        blending: additive ? AdditiveBlending : NormalBlending,
      }),
    [additive],
  );

  useEffect(() => {
    function updateTheme() {
      const dark = document.documentElement.classList.contains("dark");
      setIsDark(dark);
      material.uniforms.u_edgeColor.value = dark
        ? [0.345, 0.357, 0.439]
        : [0.612, 0.627, 0.690];
      // Galaxy tinting is a dark-mode treatment; light mode keeps calm gray
      material.uniforms.u_tintAmp.value =
        dark && buffers.edgeCount <= ADDITIVE_EDGE_LIMIT ? 1 : 0;
    }
    updateTheme();
    const observer = new MutationObserver(updateTheme);
    observer.observe(document.documentElement, { attributes: true, attributeFilter: ["class"] });
    return () => observer.disconnect();
  }, [material, buffers.edgeCount]);

  const geometry = useMemo(() => new PlaneGeometry(1, 1), []);

  useEffect(() => {
    const mesh = meshRef.current;
    if (!mesh) return;

    const { edgePositions, edgeColors, edgeTints, edgeCount } = buffers;

    const colorA = new Float32Array(edgeCount * 3);
    const colorB = new Float32Array(edgeCount * 3);
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

      colorA[i * 3 + 0] = edgeColors[i * 6 + 0];
      colorA[i * 3 + 1] = edgeColors[i * 6 + 1];
      colorA[i * 3 + 2] = edgeColors[i * 6 + 2];
      colorB[i * 3 + 0] = edgeColors[i * 6 + 3];
      colorB[i * 3 + 1] = edgeColors[i * 6 + 4];
      colorB[i * 3 + 2] = edgeColors[i * 6 + 5];
    }

    mesh.instanceMatrix.needsUpdate = true;

    mesh.geometry.setAttribute(
      "aStrength",
      new InstancedBufferAttribute(new Float32Array(edgeCount).fill(1), 1),
    );
    mesh.geometry.setAttribute("aColorA", new InstancedBufferAttribute(colorA, 3));
    mesh.geometry.setAttribute("aColorB", new InstancedBufferAttribute(colorB, 3));
    mesh.geometry.setAttribute(
      "aTint",
      new InstancedBufferAttribute(edgeTints.slice(), 1),
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
      key={additive ? "additive" : "normal"}
      ref={meshRef}
      args={[geometry, material, buffers.edgeCount]}
      frustumCulled={false}
      renderOrder={-1}
    />
  );
}
