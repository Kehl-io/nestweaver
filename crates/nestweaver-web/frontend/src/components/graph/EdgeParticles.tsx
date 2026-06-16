import { useRef, useEffect, useMemo } from "react";
import { useFrame } from "@react-three/fiber";
import { Points, BufferGeometry, Float32BufferAttribute, ShaderMaterial } from "three";
import type { GraphBuffers } from "../../hooks/useGraphBridge";

interface Props {
  buffers: GraphBuffers;
}

// Each edge gets one particle. The particle position is interpolated between
// source and target based on time: position = mix(source, target, fract(time * speed + phase))

const vertexShader = `
  attribute vec3 aSource;
  attribute vec3 aTarget;
  attribute float aPhase;

  uniform float u_time;
  uniform vec3 u_particleColor;

  varying vec3 v_color;
  varying float v_alpha;

  void main() {
    float t = fract(u_time * 0.3 + aPhase);
    vec3 pos = mix(aSource, aTarget, t);

    float centerDist = abs(t - 0.5) * 2.0;
    v_alpha = exp(-3.0 * centerDist * centerDist);
    v_color = u_particleColor;

    vec4 mvPosition = modelViewMatrix * vec4(pos, 1.0);
    gl_PointSize = 2.0 * (300.0 / -mvPosition.z);
    gl_Position = projectionMatrix * mvPosition;
  }
`;

const fragmentShader = `
  varying vec3 v_color;
  varying float v_alpha;

  void main() {
    float dist = length(gl_PointCoord - vec2(0.5));
    if (dist > 0.5) discard;
    float alpha = v_alpha * smoothstep(0.5, 0.3, dist);
    gl_FragColor = vec4(v_color, alpha * 0.4);
  }
`;

export function EdgeParticles({ buffers }: Props) {
  const pointsRef = useRef<Points>(null);
  const materialRef = useRef<ShaderMaterial>(null);

  // Build per-particle attributes from edge data
  useEffect(() => {
    const points = pointsRef.current;
    if (!points || buffers.edgeCount === 0) return;

    const n = buffers.edgeCount;
    const sources = new Float32Array(n * 3);
    const targets = new Float32Array(n * 3);
    const phases = new Float32Array(n);
    const positions = new Float32Array(n * 3); // dummy positions for Points

    for (let i = 0; i < n; i++) {
      // Source position
      sources[i * 3] = buffers.edgePositions[i * 6];
      sources[i * 3 + 1] = buffers.edgePositions[i * 6 + 1];
      sources[i * 3 + 2] = buffers.edgePositions[i * 6 + 2];
      // Target position
      targets[i * 3] = buffers.edgePositions[i * 6 + 3];
      targets[i * 3 + 1] = buffers.edgePositions[i * 6 + 4];
      targets[i * 3 + 2] = buffers.edgePositions[i * 6 + 5];
      // Phase: deterministic per-edge
      phases[i] = (i * 0.618) % 1.0; // golden ratio for nice distribution
      // Dummy position (overridden by shader)
      positions[i * 3] = sources[i * 3];
      positions[i * 3 + 1] = sources[i * 3 + 1];
      positions[i * 3 + 2] = sources[i * 3 + 2];
    }

    const geo = points.geometry as BufferGeometry;
    geo.setAttribute("position", new Float32BufferAttribute(positions, 3));
    geo.setAttribute("aSource", new Float32BufferAttribute(sources, 3));
    geo.setAttribute("aTarget", new Float32BufferAttribute(targets, 3));
    geo.setAttribute("aPhase", new Float32BufferAttribute(phases, 1));
    geo.computeBoundingSphere();
  }, [buffers]);

  // Animate time uniform
  useFrame(({ clock }) => {
    if (materialRef.current) {
      materialRef.current.uniforms.u_time.value = clock.elapsedTime;
    }
  });

  const uniforms = useMemo(() => ({
    u_time: { value: 0.0 },
    u_particleColor: { value: [0.498, 0.518, 0.612] },
  }), []);

  useEffect(() => {
    function updateColor() {
      const isDark = document.documentElement.classList.contains("dark");
      if (materialRef.current) {
        materialRef.current.uniforms.u_particleColor.value = isDark
          ? [0.498, 0.518, 0.612]
          : [0.486, 0.498, 0.576];
      }
    }
    updateColor();
    const observer = new MutationObserver(updateColor);
    observer.observe(document.documentElement, { attributes: true, attributeFilter: ["class"] });
    return () => observer.disconnect();
  }, []);

  if (buffers.edgeCount === 0) return null;

  return (
    <points ref={pointsRef} frustumCulled={false}>
      <bufferGeometry />
      <shaderMaterial
        ref={materialRef}
        vertexShader={vertexShader}
        fragmentShader={fragmentShader}
        uniforms={uniforms}
        transparent
        depthWrite={false}
      />
    </points>
  );
}
