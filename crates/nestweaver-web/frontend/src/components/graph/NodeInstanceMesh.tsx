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
    vec2 uv = v_uv - 0.5;
    float dist = length(uv) * 2.0;

    // SDF circle — crisp edge
    float circle = 1.0 - smoothstep(0.82, 0.88, dist);

    // Radial gradient: bright center → saturated rim (keeps color visible)
    float t = clamp(dist / 0.82, 0.0, 1.0);
    vec3 fillColor = v_color * mix(1.3, 0.85, t);

    // Highlight: selected/hovered nodes burn brighter
    fillColor *= (1.0 + v_highlight * 1.0);

    // Outer glow: wide, soft, vivid — the "fierce" halo
    float glowDist = max(0.0, dist - 0.7);
    float glow = exp(-4.5 * glowDist) * 0.35;

    // Inner core bloom: adds depth, not overwhelming
    float core = exp(-4.0 * dist) * 0.1;

    vec3 color = fillColor * circle + v_color * (glow + core);
    float alpha = max(circle, max(glow, core));

    if (alpha < 0.005) discard;
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
      u_breatheAmp: { value: reducedMotion ? 0 : 0.02 },
    }),
    // eslint-disable-next-line react-hooks/exhaustive-deps
    [],
  );

  // Sync reducedMotion -> uniform
  useEffect(() => {
    uniforms.u_breatheAmp.value = reducedMotion ? 0 : 0.02;
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

  // Impact ripple: briefly highlight neighbors when a node is selected
  useEffect(() => {
    if (!selectedNodeId || !graphInstance) return;

    const mesh = meshRef.current;
    if (!mesh) return;

    const highlightAttr = mesh.geometry.getAttribute("aHighlight") as InstancedBufferAttribute | undefined;
    if (!highlightAttr) return;

    const neighborUids: string[] = [];
    try {
      graphInstance.neighbors(selectedNodeId).forEach((n) => neighborUids.push(n));
    } catch {
      return;
    }

    if (neighborUids.length === 0) return;

    // Apply partial glow to each neighbor
    for (const uid of neighborUids) {
      const idx = buffers.uidToIndex.get(uid);
      if (idx !== undefined) {
        // Ripple on top of any existing highlight value — use max so selected node keeps full 1.0
        const current = highlightAttr.getX(idx);
        highlightAttr.setX(idx, Math.max(current, 0.5));
      }
    }
    highlightAttr.needsUpdate = true;

    // Clear ripple after 300ms — the hover/selection effect will re-run and restore correct state
    const timer = setTimeout(() => {
      const m = meshRef.current;
      if (!m) return;
      const attr = m.geometry.getAttribute("aHighlight") as InstancedBufferAttribute | undefined;
      if (!attr) return;
      for (const uid of neighborUids) {
        const idx = buffers.uidToIndex.get(uid);
        if (idx !== undefined) {
          // Only clear the ripple boost; if a neighbor is also the hovered/selected node keep 1.0
          const { uidToIndex } = buffers;
          const hovIdx = hoveredNodeId !== null ? (uidToIndex.get(hoveredNodeId) ?? -1) : -1;
          const selIdx = selectedNodeId !== null ? (uidToIndex.get(selectedNodeId) ?? -1) : -1;
          const keep = idx === hovIdx || idx === selIdx ? 1.0 : 0.0;
          attr.setX(idx, keep);
        }
      }
      attr.needsUpdate = true;
    }, 300);

    return () => clearTimeout(timer);
  // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [selectedNodeId]);

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
