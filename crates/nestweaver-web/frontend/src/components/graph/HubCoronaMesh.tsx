import { useRef, useEffect, useMemo } from "react";
import { useFrame } from "@react-three/fiber";
import {
  AdditiveBlending,
  InstancedMesh,
  InstancedBufferAttribute,
  Object3D,
} from "three";
import type { GraphBuffers } from "../../hooks/useGraphBridge";

// Kind-tinted additive coronas for hub-tier nodes only (importance >= 0.6)
// — one extra instanced layer with tight quads (2.6x node radius) to bound
// overdraw. Gentle breathing on hubs is a sanctioned idle motion; it zeroes
// out under reduced motion.

const CORONA_SCALE = 2.6;
const IMPORTANCE_FLOOR = 0.6;

const vertexShader = /* glsl */ `
attribute float aSize;
attribute vec3 aColor;
attribute float aPhase;
attribute float aStrength;

uniform float u_time;
uniform float u_breatheAmp;

varying vec2 v_uv;
varying vec3 v_color;
varying float v_strength;

void main() {
    v_uv = uv;
    v_color = aColor;
    v_strength = aStrength;

    float breathe = 1.0 + u_breatheAmp * 0.03 * sin(u_time * 1.6 + aPhase * 6.2831);
    float scale = aSize * 0.45 * ${CORONA_SCALE.toFixed(1)} * breathe;
    vec3 pos = position * scale;
    vec4 mvPosition = modelViewMatrix * instanceMatrix * vec4(pos, 1.0);
    gl_Position = projectionMatrix * mvPosition;
}
`;

const fragmentShader = /* glsl */ `
varying vec2 v_uv;
varying vec3 v_color;
varying float v_strength;

void main() {
    vec2 uv = v_uv - 0.5;
    float dist = length(uv) * 2.0;
    float glow = exp(-3.0 * dist * dist) * 0.32 * v_strength;
    if (glow < 0.008) discard;
    gl_FragColor = vec4(v_color * glow, glow);
}
`;

interface Props {
  buffers: GraphBuffers;
  reducedMotion: boolean;
}

export function HubCoronaMesh({ buffers, reducedMotion }: Props) {
  const meshRef = useRef<InstancedMesh>(null);
  const tempObj = useMemo(() => new Object3D(), []);

  const hubIndices = useMemo(() => {
    const indices: number[] = [];
    for (let i = 0; i < buffers.nodeCount; i++) {
      if (buffers.importance[i] >= IMPORTANCE_FLOOR) indices.push(i);
    }
    return indices;
  }, [buffers]);

  const uniforms = useMemo(
    () => ({
      u_time: { value: 0 },
      u_breatheAmp: { value: reducedMotion ? 0 : 1 },
    }),
    // eslint-disable-next-line react-hooks/exhaustive-deps
    [],
  );

  useEffect(() => {
    uniforms.u_breatheAmp.value = reducedMotion ? 0 : 1;
  }, [reducedMotion, uniforms]);

  useFrame(({ clock }) => {
    uniforms.u_time.value = clock.getElapsedTime();
  });

  useEffect(() => {
    const mesh = meshRef.current;
    if (!mesh || hubIndices.length === 0) return;

    const n = hubIndices.length;
    const sizes = new Float32Array(n);
    const colors = new Float32Array(n * 3);
    const phases = new Float32Array(n);
    const strengths = new Float32Array(n);

    hubIndices.forEach((nodeIdx, i) => {
      tempObj.position.set(
        buffers.positions[nodeIdx * 3],
        buffers.positions[nodeIdx * 3 + 1],
        buffers.positions[nodeIdx * 3 + 2] - 0.05,
      );
      tempObj.scale.setScalar(1);
      tempObj.rotation.set(0, 0, 0);
      tempObj.updateMatrix();
      mesh.setMatrixAt(i, tempObj.matrix);

      sizes[i] = buffers.sizes[nodeIdx];
      colors[i * 3] = buffers.colors[nodeIdx * 3];
      colors[i * 3 + 1] = buffers.colors[nodeIdx * 3 + 1];
      colors[i * 3 + 2] = buffers.colors[nodeIdx * 3 + 2];
      phases[i] = buffers.phases[nodeIdx];
      strengths[i] = (buffers.importance[nodeIdx] - IMPORTANCE_FLOOR) / (1 - IMPORTANCE_FLOOR);
    });

    mesh.instanceMatrix.needsUpdate = true;
    mesh.geometry.setAttribute("aSize", new InstancedBufferAttribute(sizes, 1));
    mesh.geometry.setAttribute("aColor", new InstancedBufferAttribute(colors, 3));
    mesh.geometry.setAttribute("aPhase", new InstancedBufferAttribute(phases, 1));
    mesh.geometry.setAttribute("aStrength", new InstancedBufferAttribute(strengths, 1));
  }, [buffers, hubIndices, tempObj]);

  if (hubIndices.length === 0) return null;

  return (
    <instancedMesh
      key={hubIndices.length}
      ref={meshRef}
      args={[undefined, undefined, hubIndices.length]}
      frustumCulled={false}
      renderOrder={-2}
    >
      <planeGeometry args={[2, 2]} />
      <shaderMaterial
        uniforms={uniforms}
        vertexShader={vertexShader}
        fragmentShader={fragmentShader}
        transparent
        depthWrite={false}
        toneMapped={false}
        blending={AdditiveBlending}
      />
    </instancedMesh>
  );
}
