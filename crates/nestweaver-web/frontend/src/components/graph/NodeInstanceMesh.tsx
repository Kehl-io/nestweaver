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
attribute float aImportance;
attribute float aSeed;
attribute float aBridge;

uniform float u_time;
uniform float u_breatheAmp;
uniform float u_motionAmp;
uniform float u_intro;

varying vec2 v_uv;
varying vec3 v_color;
varying float v_highlight;
varying float v_importance;
varying float v_seed;
varying float v_bridge;
varying float v_phase;

void main() {
    v_uv = uv;
    v_color = aColor;
    v_highlight = aHighlight;
    v_importance = aImportance;
    v_seed = aSeed;
    v_bridge = aBridge;
    v_phase = aPhase;

    // Nodes arrive with a quick scale-in while staying locked to their graph coordinates.
    float intro = clamp(u_intro, 0.0, 1.0);
    float rebound = sin(intro * 3.14159) * (1.0 - intro) * 0.08 * u_motionAmp;
    float introScale = mix(0.62, 1.0, intro) + rebound;
    float breathe = 1.0 + u_breatheAmp * sin(u_time * 0.8 + aPhase * 6.2831);
    float focusLift = 1.0 + aHighlight * 0.18 + aSeed * 0.04;
    float beaconScale = 0.62 + aImportance * 0.06;
    float scale = aSize * beaconScale * introScale * breathe * focusLift;

    vec3 pos = position * scale;

    vec4 mvPosition = modelViewMatrix * instanceMatrix * vec4(pos, 1.0);
    gl_Position = projectionMatrix * mvPosition;
}
`;

const fragmentShader = /* glsl */ `
varying vec2 v_uv;
varying vec3 v_color;
varying float v_highlight;
varying float v_importance;
varying float v_seed;
varying float v_bridge;
varying float v_phase;

uniform float u_time;

void main() {
    vec2 uv = v_uv - 0.5;
    float dist = length(uv) * 2.0;

    // Obsidian-like dot: clean filled center, soft antialiasing, glow only on focus.
    float body = 1.0 - smoothstep(0.68, 0.78, dist);
    float hotCore = exp(-7.5 * dist * dist);
    float pulse = 0.5 + 0.5 * sin(u_time * 5.2 + v_phase * 6.2831);
    float focusAura = exp(-8.2 * max(0.0, dist - 0.62)) *
        v_highlight *
        (0.11 + pulse * 0.08);

    vec3 coreColor = mix(v_color * 0.96, v_color * 1.04, hotCore * 0.28);
    coreColor *= 0.98 + v_importance * 0.08 + v_highlight * 0.24 + v_seed * 0.08;

    vec3 focusInk = v_color * 1.32;

    vec3 color =
        coreColor * body +
        focusInk * hotCore * v_highlight * 0.20 +
        focusInk * focusAura;

    float halo = exp(-8.6 * max(0.0, dist - 0.74)) * (v_highlight * 0.045 + v_seed * 0.018);
    color += v_color * halo;
    float alpha = max(body, max(focusAura, halo));

    if (alpha < 0.012) discard;
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
  const graphKey = useMemo(
    () => buffers.indexToUid.join("\u0000"),
    [buffers.indexToUid],
  );

  // Shared uniforms object — stable reference so we mutate in place
  const uniforms = useMemo(
    () => ({
      u_time: { value: 0 },
      u_breatheAmp: { value: 0 },
      u_motionAmp: { value: reducedMotion ? 0 : 1 },
      u_intro: { value: reducedMotion ? 1 : 0 },
    }),
    // eslint-disable-next-line react-hooks/exhaustive-deps
    [],
  );

  // Sync reducedMotion -> uniform
  useEffect(() => {
    uniforms.u_breatheAmp.value = 0;
    uniforms.u_motionAmp.value = reducedMotion ? 0 : 1;
    if (reducedMotion) uniforms.u_intro.value = 1;
  }, [reducedMotion, uniforms]);

  const introStartRef = useRef<number | null>(null);

  useEffect(() => {
    introStartRef.current = null;
    uniforms.u_intro.value = reducedMotion ? 1 : 0;
  }, [graphKey, reducedMotion, uniforms]);

  // Tick animation uniforms every frame.
  useFrame(({ clock }) => {
    const elapsed = clock.getElapsedTime();
    uniforms.u_time.value = elapsed;
    if (!reducedMotion && uniforms.u_intro.value < 1) {
      if (introStartRef.current === null) introStartRef.current = elapsed;
      uniforms.u_intro.value = Math.min(1, (elapsed - introStartRef.current) / 1.2);
    }
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

    const {
      nodeCount,
      colors,
      sizes,
      phases,
      importance,
      seedMarkers,
      bridgeStrengths,
    } = buffers;

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

    mesh.geometry.setAttribute(
      "aImportance",
      new InstancedBufferAttribute(importance.slice(), 1),
    );

    mesh.geometry.setAttribute(
      "aSeed",
      new InstancedBufferAttribute(seedMarkers.slice(), 1),
    );

    mesh.geometry.setAttribute(
      "aBridge",
      new InstancedBufferAttribute(bridgeStrengths.slice(), 1),
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

    // Determine neighbor set of the active node (for dim-non-neighbors logic)
    const neighborSet = new Set<number>();
    const focusNodeId = hoveredNodeId ?? selectedNodeId;
    if (focusNodeId && graphInstance) {
      neighborSet.add(uidToIndex.get(focusNodeId) ?? -1);
      try {
        graphInstance.neighbors(focusNodeId).forEach((n) => {
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

    const dimming = focusNodeId !== null;

    for (let i = 0; i < nodeCount; i++) {
      const baseR = colors[i * 3];
      const baseG = colors[i * 3 + 1];
      const baseB = colors[i * 3 + 2];

      // Dim factor: pull unrelated nodes back when exploring a neighborhood.
      const isNeighbor = !dimming || neighborSet.has(i);
      const dimFactor = isNeighbor ? 1.0 : 0.64;

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
