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

    // Nodes arrive with a springy, deterministic pop, then settle into a quiet breath.
    float intro = clamp(u_intro, 0.0, 1.0);
    float rebound = sin(intro * 3.14159) * (1.0 - intro) * 0.22 * u_motionAmp;
    float introScale = mix(0.34, 1.0, intro) + rebound;
    float breathe = 1.0 + u_breatheAmp * sin(u_time * 0.8 + aPhase * 6.2831);
    float focusLift = 1.0 + aHighlight * 0.08 + aSeed * 0.035;
    float beaconScale = 1.08 + aImportance * 0.06;
    float scale = aSize * beaconScale * introScale * breathe * focusLift;

    vec3 pos = position * scale;

    // A tiny outward bounce on load makes the graph feel alive without moving nodes forever.
    vec2 arrivalDir = normalize(vec2(cos(aPhase * 6.2831), sin(aPhase * 6.2831)));
    float drift = sin((intro * 3.0 + aPhase) * 6.2831) * pow(1.0 - intro, 2.0);
    pos.xy += arrivalDir * drift * aSize * 0.85 * u_motionAmp;

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

float ring(float dist, float inner, float outer, float softness) {
    return smoothstep(inner - softness, inner + softness, dist) *
        (1.0 - smoothstep(outer - softness, outer + softness, dist));
}

void main() {
    vec2 uv = v_uv - 0.5;
    float dist = length(uv) * 2.0;
    float angle = atan(uv.y, uv.x);
    float arcPos = fract((angle + 3.14159265) / 6.2831853 + v_phase * 0.08);

    // Semantic beacon layers: matte body, etched rim, importance arc, and state rings.
    float body = 1.0 - smoothstep(0.64, 0.69, dist);
    float bevel = ring(dist, 0.54, 0.67, 0.035);
    float outerRim = ring(dist, 0.71, 0.84, 0.018);
    float fineEdge = ring(dist, 0.86, 0.9, 0.01);

    float arcAmount = mix(0.24, 0.98, clamp(v_importance, 0.0, 1.0));
    float arcMask = 1.0 - smoothstep(arcAmount - 0.025, arcAmount + 0.025, arcPos);
    float importanceArc = outerRim * arcMask;

    float pulse = 0.5 + 0.5 * sin(u_time * 5.2 + v_phase * 6.2831);
    float focusRing = ring(dist, 0.91, 0.985, 0.014) * v_highlight * (0.78 + pulse * 0.22);
    float seedRing = ring(dist, 0.91, 0.985, 0.012) * v_seed;
    float bridgeRing = ring(dist, 0.47, 0.52, 0.012) * v_bridge;
    float sparkSource = clamp(v_highlight + v_seed * 0.7, 0.0, 1.0);
    float sparkPos = fract(arcPos - u_time * 0.28);
    float sparkDistance = min(sparkPos, 1.0 - sparkPos);
    float focusSpark = exp(-260.0 * sparkDistance * sparkDistance) *
        ring(dist, 0.91, 0.985, 0.01) *
        sparkSource *
        (0.62 + pulse * 0.38);

    // A small glint gives the node dimensionality while keeping the silhouette sharp.
    float glint = exp(-72.0 * dot(uv - vec2(-0.17, 0.2), uv - vec2(-0.17, 0.2)));
    float lowerShade = smoothstep(-0.18, -0.5, uv.y) * body;

    vec3 coreColor = mix(v_color * 0.72, v_color * 1.18, 1.0 - smoothstep(0.0, 0.7, dist));
    coreColor = mix(coreColor, coreColor * 0.72, lowerShade * 0.28);
    coreColor *= 1.0 + v_highlight * 0.34 + v_seed * 0.16;

    vec3 darkInk = v_color * 0.42;
    vec3 lightInk = mix(v_color * 1.1, vec3(1.0), 0.34);
    vec3 focusInk = mix(v_color * 1.1, vec3(1.0), 0.62);

    vec3 color =
        coreColor * body +
        lightInk * bevel * 0.22 +
        darkInk * outerRim * 0.55 +
        lightInk * importanceArc * (0.56 + v_importance * 0.34) +
        focusInk * focusRing +
        focusInk * seedRing * 0.86 +
        vec3(1.0) * focusSpark * 0.9 +
        lightInk * bridgeRing * 0.62 +
        vec3(1.0) * glint * body * 0.2 +
        lightInk * fineEdge * 0.38;

    float halo = exp(-16.0 * max(0.0, dist - 0.93)) * (v_highlight * 0.09 + v_seed * 0.045);
    color += v_color * halo;
    float alpha = max(body, max(outerRim * 0.8, max(fineEdge, max(focusRing, max(seedRing, max(bridgeRing, max(focusSpark, halo)))))));

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

  // Shared uniforms object — stable reference so we mutate in place
  const uniforms = useMemo(
    () => ({
      u_time: { value: 0 },
      u_breatheAmp: { value: reducedMotion ? 0 : 0.018 },
      u_motionAmp: { value: reducedMotion ? 0 : 1 },
      u_intro: { value: reducedMotion ? 1 : 0 },
    }),
    // eslint-disable-next-line react-hooks/exhaustive-deps
    [],
  );

  // Sync reducedMotion -> uniform
  useEffect(() => {
    uniforms.u_breatheAmp.value = reducedMotion ? 0 : 0.018;
    uniforms.u_motionAmp.value = reducedMotion ? 0 : 1;
    if (reducedMotion) uniforms.u_intro.value = 1;
  }, [reducedMotion, uniforms]);

  const introStartRef = useRef<number | null>(null);

  useEffect(() => {
    introStartRef.current = null;
    uniforms.u_intro.value = reducedMotion ? 1 : 0;
  }, [buffers, reducedMotion, uniforms]);

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
