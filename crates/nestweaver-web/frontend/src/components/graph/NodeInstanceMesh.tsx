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
attribute float aIgniteDelay;

uniform float u_time;
uniform float u_breatheAmp;
uniform float u_motionAmp;
uniform float u_intro;
uniform vec2 u_rippleOrigin;
uniform float u_rippleStart;

varying vec2 v_uv;
varying vec3 v_color;
varying float v_highlight;
varying float v_bloom;
varying float v_importance;
varying float v_seed;
varying float v_bridge;
varying float v_phase;
varying float v_ripple;

void main() {
    v_uv = uv;
    v_color = aColor;
    v_highlight = aHighlight;
    v_importance = aImportance;
    v_seed = aSeed;
    v_bridge = aBridge;
    v_phase = aPhase;

    // Ignition: u_intro is a 0..1 clock over the whole choreography; each
    // node ramps in over 0.35 of the clock after its own delay (hubs first,
    // members staggered inside the verified dwell<=0.4 band — one-time load
    // theater only, nothing is trackable yet)
    float ignite = clamp((clamp(u_intro, 0.0, 1.0) - aIgniteDelay) / 0.35, 0.0, 1.0);
    // Cubic ease-in-out (slow-in/slow-out per the HCI evidence)
    ignite = ignite * ignite * (3.0 - 2.0 * ignite);

    // Hub supernova: importance-weighted overshoot that blows the core out
    // near-white as it arrives, easing back as ignition completes
    float supernova = sin(ignite * 3.14159) * aImportance * 0.9 * u_motionAmp;

    v_bloom = clamp(
        aHighlight * 1.0 +
        aBridge * 0.55 +
        pow(max(aImportance, 0.0), 1.35) * 0.38 +
        aSeed * 0.45 +
        supernova,
        0.0,
        1.4
    );

    // Focus impact-ripple: one synchronized luminance wave expanding from
    // the focused node (800ms cubic-out; no stagger — this is a tracking
    // moment). Inactive when u_rippleStart < 0.
    v_ripple = 0.0;
    if (u_rippleStart >= 0.0) {
      float age = u_time - u_rippleStart;
      if (age >= 0.0 && age < 0.8) {
        float t = age / 0.8;
        float eased = 1.0 - pow(1.0 - t, 3.0);
        vec2 nodePos = (instanceMatrix * vec4(0.0, 0.0, 0.0, 1.0)).xy;
        float d = distance(nodePos, u_rippleOrigin);
        float front = eased * 260.0;
        float band = exp(-pow(d - front, 2.0) / (2.0 * 42.0 * 42.0));
        v_ripple = band * (1.0 - t) * u_motionAmp;
      }
    }

    float rebound = sin(ignite * 3.14159) * (1.0 - ignite) * 0.08 * u_motionAmp;
    float introScale = mix(0.55, 1.0, ignite) + rebound;
    float breathe = 1.0 + u_breatheAmp * sin(u_time * 0.8 + aPhase * 6.2831);
    float focusLift = 1.0 + aHighlight * 0.18 + aSeed * 0.04;
    float baseScale = 0.45;
    // Quads are enlarged 1.6x purely for the ambient halo ring; the disc
    // body keeps its world size (fragment dist is compensated). Picking is
    // CPU-side over aSize, so hit areas are unaffected.
    float scale = aSize * baseScale * introScale * breathe * focusLift * 1.6;

    vec3 pos = position * scale;

    vec4 mvPosition = modelViewMatrix * instanceMatrix * vec4(pos, 1.0);
    gl_Position = projectionMatrix * mvPosition;
}
`;

const fragmentShader = /* glsl */ `
varying vec2 v_uv;
varying vec3 v_color;
varying float v_highlight;
varying float v_bloom;
varying float v_importance;
varying float v_seed;
varying float v_bridge;
varying float v_phase;
varying float v_ripple;

uniform float u_time;
uniform vec3 u_strokeColor;
uniform float u_haloAmp;
uniform float u_motionAmp;

void main() {
    vec2 uv = v_uv - 0.5;
    // Quad is 1.6x oversized for the halo; dist=1.0 is the disc edge
    float dist = length(uv) * 3.2;

    float body = 1.0 - smoothstep(0.92, 0.985, dist);
    float core = exp(-3.5 * dist * dist);
    float rim = smoothstep(0.56, 0.94, dist) * (1.0 - smoothstep(0.94, 0.995, dist));
    float important = smoothstep(0.48, 1.0, v_importance);
    float bridge = smoothstep(0.2, 1.0, v_bridge);
    float bloom = clamp(v_bloom + v_ripple * 0.5, 0.0, 1.4);

    vec3 color = v_color * (1.05 + v_highlight * 0.22 + important * 0.15);
    // Hue-tinted near-white core; importance lifts core luminance (design:
    // "big = important" reads as a hotter core, body keeps the kind hue)
    vec3 coreColor = mix(color, vec3(1.0), 0.18 + bloom * 0.30 + important * 0.14);
    color = mix(color, coreColor, core * (0.20 + bloom * 0.45));

    // Rim darkens the node's own hue — the accent cyan is reserved for
    // selection and bloom-tier emphasis, never a broad tint on every node
    vec3 rimColor = color * 0.68;
    color = mix(color, rimColor, rim * 0.30);
    color += u_strokeColor * bloom * (core * 0.20 + rim * 0.26) * smoothstep(0.35, 1.0, bloom);
    // Emissive lift above 1.0 on loud nodes feeds the HDR-selective bloom
    // pass (threshold ~1.0); ambient nodes stay below and never bloom.
    // The ripple wave rides the same channel as a brief luminance kick.
    color *= 1.0 + bloom * bloom * 1.6 + v_ripple * 0.6;

    // Selection stroke ring: thin annulus at edge
    float ringInner = smoothstep(0.78, 0.82, dist);
    float ringOuter = 1.0 - smoothstep(0.92, 0.985, dist);
    float ring = ringInner * ringOuter * v_highlight;
    color = mix(color, u_strokeColor, ring * 0.92);

    // Ambient halo outside the disc: the below-bloom shimmer that makes the
    // resting field feel alive (design: universal glow floor, density-gated
    // via u_haloAmp; dark mode only)
    float halo = exp(-2.4 * max(dist - 0.95, 0.0)) * step(0.95, dist);
    float haloAlpha = halo * 0.22 * u_haloAmp * (0.6 + 0.4 * v_importance);

    float alpha = max(body, ring) + haloAlpha;
    vec3 haloColor = v_color * (0.85 + bloom * 0.3);
    color = mix(haloColor, color, clamp(body + ring, 0.0, 1.0));

    // Bridge convergence ring: Spark green (the reserved hue), slowly
    // contracting inward — betweenness made literal, "flow pulled in from
    // all directions". Freezes at mid-phase under reduced motion.
    if (v_bridge > 0.5) {
      float phase = mix(0.5, fract(u_time * 0.22 + v_seed), u_motionAmp);
      float radius = mix(1.45, 0.95, phase);
      float bridgeRing = smoothstep(0.10, 0.0, abs(dist - radius)) * clamp((v_bridge - 0.5) * 2.0, 0.0, 1.0);
      vec3 spark = vec3(0.302, 1.0, 0.0);
      // Above-1.0 contribution joins the bloom set (locked decision)
      color += spark * bridgeRing * 1.25;
      alpha = max(alpha, bridgeRing * 0.85);
    }

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
      u_strokeColor: { value: [0.369, 0.816, 0.996] },
      u_haloAmp: { value: 0 },
      u_rippleOrigin: { value: [0, 0] },
      u_rippleStart: { value: -1 },
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

  // Theme-reactive stroke color + halo gate (dark mode only; halo also
  // gates off on very dense scenes to bound additive fill cost)
  useEffect(() => {
    function updateStroke() {
      const isDark = document.documentElement.classList.contains("dark");
      uniforms.u_strokeColor.value = isDark
        ? [0.369, 0.816, 0.996]
        : [0.031, 0.384, 0.655];
      uniforms.u_haloAmp.value = isDark && buffers.nodeCount <= 2000 ? 1 : 0;
    }
    updateStroke();
    const observer = new MutationObserver(updateStroke);
    observer.observe(document.documentElement, { attributes: true, attributeFilter: ["class"] });
    return () => observer.disconnect();
  }, [uniforms, buffers.nodeCount]);

  const introStartRef = useRef<number | null>(null);

  useEffect(() => {
    introStartRef.current = null;
    uniforms.u_intro.value = reducedMotion ? 1 : 0;
  }, [graphKey, reducedMotion, uniforms]);

  // Tick animation uniforms every frame. The ignition choreography spans
  // 2.0s (3 broad stages driven by per-node delays — verified staging cap).
  useFrame(({ clock }) => {
    const elapsed = clock.getElapsedTime();
    uniforms.u_time.value = elapsed;
    if (!reducedMotion && uniforms.u_intro.value < 1) {
      if (introStartRef.current === null) introStartRef.current = elapsed;
      uniforms.u_intro.value = Math.min(1, (elapsed - introStartRef.current) / 2.0);
    }
  });

  // Focus impact-ripple: one synchronized wave from the newly selected node.
  // Keyboard selection triggers identically to click (a11y parity); reduced
  // motion keeps the existing static neighbor emphasis instead.
  const clockRef = useRef<{ getElapsedTime: () => number } | null>(null);
  useFrame(({ clock }) => {
    clockRef.current = clock;
  });
  useEffect(() => {
    if (!selectedNodeId || reducedMotion) return;
    const idx = buffers.uidToIndex.get(selectedNodeId);
    if (idx === undefined) return;
    uniforms.u_rippleOrigin.value = [
      buffers.positions[idx * 3],
      buffers.positions[idx * 3 + 1],
    ];
    uniforms.u_rippleStart.value = clockRef.current?.getElapsedTime() ?? -1;
  // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [selectedNodeId, reducedMotion]);

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

    // aIgniteDelay: hubs (seeds/high-importance) arrive first; members
    // stagger inside dwell 0.25-0.60 of the ignition clock — one-time
    // load choreography only (staggering harms tracking; nothing is
    // trackable during first paint)
    const igniteDelays = new Float32Array(nodeCount);
    for (let i = 0; i < nodeCount; i++) {
      const isHubLike = seedMarkers[i] === 1 || importance[i] > 0.7;
      igniteDelays[i] = isHubLike ? 0 : 0.25 + phases[i] * 0.35;
    }
    mesh.geometry.setAttribute(
      "aIgniteDelay",
      new InstancedBufferAttribute(igniteDelays, 1),
    );

    // aHighlight — initially all zero
    mesh.geometry.setAttribute(
      "aHighlight",
      new InstancedBufferAttribute(new Float32Array(nodeCount), 1),
    );
  }, [buffers]);

  // (The former 300ms JS neighbor-highlight ripple was superseded by the
  // shader-driven focus impact-ripple above — one wave, no attr churn.)

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
