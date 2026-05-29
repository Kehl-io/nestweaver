import { useRef, useEffect, useMemo } from "react";
import { useFrame } from "@react-three/fiber";
import {
  InstancedMesh,
  Object3D,
  InstancedBufferAttribute,
  ShaderMaterial,
} from "three";
import type { GraphBuffers } from "../../hooks/useGraphBridge";
import { useStore } from "../../stores";

// ---- GLSL shaders ----

const vertexShader = /* glsl */ `
attribute float aPhase;
attribute float aSize;
attribute vec3 aColor;
attribute float aHighlight;

uniform float u_time;
uniform float u_breatheAmp;

varying vec2 v_uv;
varying vec3 v_color;
varying float v_highlight;

void main() {
    v_uv = uv;
    v_color = aColor;
    v_highlight = aHighlight;

    // Breathing: per-instance scale oscillation (disabled when u_breatheAmp == 0)
    float breathe = 1.0 + u_breatheAmp * sin(u_time * 0.8 + aPhase * 6.2831);
    float scale = aSize * breathe;

    // Scale the quad in local space
    vec3 pos = position * scale;

    // Apply instance matrix (position only — scale handled above)
    vec4 mvPosition = modelViewMatrix * instanceMatrix * vec4(pos, 1.0);
    gl_Position = projectionMatrix * mvPosition;
}
`;

const fragmentShader = /* glsl */ `
varying vec2 v_uv;
varying vec3 v_color;
varying float v_highlight;

void main() {
    // Center the UV coordinates: [-0.5, 0.5]
    vec2 uv = v_uv - 0.5;
    float dist = length(uv) * 2.0;  // 0 at center, 1 at quad edge

    // SDF circle with anti-aliasing
    float circle = 1.0 - smoothstep(0.85, 0.9, dist);

    // Radial gradient: bright center -> darker rim
    // dist/0.85 is 0 at center, 1 at circle edge
    float t = clamp(dist / 0.85, 0.0, 1.0);
    vec3 fillColor = v_color * mix(1.35, 0.85, t);

    // Selection/hover highlight: boost above bloom luminance threshold
    fillColor = fillColor * (1.0 + v_highlight * 0.8);

    // Outer glow: exponential falloff past circle edge
    float glowDist = max(0.0, dist - 0.75);
    float glow = exp(-6.0 * glowDist) * 0.25;

    // Combine fill and glow
    vec3 color = fillColor * circle + v_color * glow;
    float alpha = max(circle, glow);

    if (alpha < 0.01) discard;
    gl_FragColor = vec4(color, alpha);
}
`;

// ---- Component ----

interface Props {
  buffers: GraphBuffers;
  reducedMotion?: boolean;
}

export function NodeInstanceMesh({ buffers, reducedMotion = false }: Props) {
  const meshRef = useRef<InstancedMesh>(null);
  const matRef = useRef<ShaderMaterial>(null);
  const tempObject = useMemo(() => new Object3D(), []);

  const selectedNodeId = useStore((s) => s.selectedNodeId);
  const hoveredNodeId = useStore((s) => s.hoveredNodeId);
  const graphInstance = useStore((s) => s.graphInstance);

  // Shared uniforms object — stable reference so we mutate in place
  const uniforms = useMemo(
    () => ({
      u_time: { value: 0 },
      u_breatheAmp: { value: reducedMotion ? 0 : 0.015 },
    }),
    // eslint-disable-next-line react-hooks/exhaustive-deps
    [],
  );

  // Sync reducedMotion -> uniform
  useEffect(() => {
    uniforms.u_breatheAmp.value = reducedMotion ? 0 : 0.015;
  }, [reducedMotion, uniforms]);

  // Tick u_time every frame
  useFrame(({ clock }) => {
    uniforms.u_time.value = clock.getElapsedTime();
  });

  // Update instance matrices when positions change.
  // Scale is now handled in the shader — matrix only encodes position.
  useEffect(() => {
    const mesh = meshRef.current;
    if (!mesh) return;

    const { positions, nodeCount } = buffers;

    for (let i = 0; i < nodeCount; i++) {
      const x = positions[i * 3];
      const y = positions[i * 3 + 1];
      const z = positions[i * 3 + 2];

      tempObject.position.set(x, y, z);
      tempObject.scale.setScalar(1); // scale is in shader
      tempObject.rotation.set(0, 0, 0);
      tempObject.updateMatrix();
      mesh.setMatrixAt(i, tempObject.matrix);
    }
    mesh.instanceMatrix.needsUpdate = true;
  }, [buffers, tempObject]);

  // Update per-instance aColor, aSize, aPhase attributes from base buffers
  useEffect(() => {
    const mesh = meshRef.current;
    if (!mesh) return;

    const { nodeCount, colors, sizes, phases } = buffers;

    // aColor
    mesh.geometry.setAttribute(
      "aColor",
      new InstancedBufferAttribute(colors.slice(), 3),
    );

    // aSize
    mesh.geometry.setAttribute(
      "aSize",
      new InstancedBufferAttribute(sizes.slice(), 1),
    );

    // aPhase
    mesh.geometry.setAttribute(
      "aPhase",
      new InstancedBufferAttribute(phases.slice(), 1),
    );

    // aHighlight — initially all zero
    mesh.geometry.setAttribute(
      "aHighlight",
      new InstancedBufferAttribute(new Float32Array(nodeCount), 1),
    );
  }, [buffers]);

  // Update aColor, aSize, aHighlight when hover/selection changes
  useEffect(() => {
    const mesh = meshRef.current;
    if (!mesh) return;

    const { nodeCount, colors, sizes, uidToIndex } = buffers;

    // Determine neighbor set of hovered node (for dim-non-neighbors logic)
    const neighborSet = new Set<number>();
    if (hoveredNodeId && graphInstance) {
      neighborSet.add(uidToIndex.get(hoveredNodeId) ?? -1);
      try {
        graphInstance.neighbors(hoveredNodeId).forEach((n) => {
          const idx = uidToIndex.get(n);
          if (idx !== undefined) neighborSet.add(idx);
        });
      } catch {
        // node might not exist in graph instance
      }
    }

    const hoveredIdx =
      hoveredNodeId !== null ? (uidToIndex.get(hoveredNodeId) ?? -1) : -1;
    const selectedIdx =
      selectedNodeId !== null ? (uidToIndex.get(selectedNodeId) ?? -1) : -1;

    const colorAttr = mesh.geometry.getAttribute("aColor") as InstancedBufferAttribute | undefined;
    const sizeAttr = mesh.geometry.getAttribute("aSize") as InstancedBufferAttribute | undefined;
    const highlightAttr = mesh.geometry.getAttribute("aHighlight") as InstancedBufferAttribute | undefined;

    if (!colorAttr || !sizeAttr || !highlightAttr) return;

    const dimming = hoveredNodeId !== null;

    for (let i = 0; i < nodeCount; i++) {
      const baseR = colors[i * 3];
      const baseG = colors[i * 3 + 1];
      const baseB = colors[i * 3 + 2];

      // Dim factor: 0.15 for non-neighbors when hovering
      const isNeighbor = !dimming || neighborSet.has(i);
      const dimFactor = isNeighbor ? 1.0 : 0.15;

      // Size modifiers
      let sizeMult = 1.0;
      if (i === hoveredIdx) sizeMult = 1.15;
      else if (i === selectedIdx) sizeMult = 1.08;

      colorAttr.setXYZ(i, baseR * dimFactor, baseG * dimFactor, baseB * dimFactor);
      sizeAttr.setX(i, sizes[i] * sizeMult);

      // Highlight (drives bloom in fragment shader): hovered or selected
      const isHighlighted = i === hoveredIdx || i === selectedIdx ? 1.0 : 0.0;
      highlightAttr.setX(i, isHighlighted);
    }

    colorAttr.needsUpdate = true;
    sizeAttr.needsUpdate = true;
    highlightAttr.needsUpdate = true;
  }, [buffers, hoveredNodeId, selectedNodeId, graphInstance]);

  if (buffers.nodeCount === 0) return null;

  return (
    <instancedMesh
      ref={meshRef}
      args={[undefined, undefined, buffers.nodeCount]}
      frustumCulled={false}
    >
      {/* Unit quad [-1,1] — shader scales via aSize */}
      <planeGeometry args={[2, 2]} />
      <shaderMaterial
        ref={matRef}
        uniforms={uniforms}
        vertexShader={vertexShader}
        fragmentShader={fragmentShader}
        transparent
        depthWrite={false}
        toneMapped={false}
      />
    </instancedMesh>
  );
}
