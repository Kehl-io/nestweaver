import { useMemo, useRef } from "react";
import { useFrame } from "@react-three/fiber";
import { ShaderMaterial } from "three";

// Procedural FBM nebula backdrop — zero external assets (CSP-safe), brand
// Dusk/Cobalt hues at very low luminance so it reads as "a place" without
// competing with node marks. Sits far behind the graph plane; excluded from
// bloom by staying far below the luminance threshold.

const vertexShader = /* glsl */ `
  varying vec2 v_uv;
  void main() {
    v_uv = uv;
    gl_Position = projectionMatrix * modelViewMatrix * vec4(position, 1.0);
  }
`;

const fragmentShader = /* glsl */ `
  uniform float u_time;
  uniform float u_drift;

  varying vec2 v_uv;

  float hash(vec2 p) {
    return fract(sin(dot(p, vec2(127.1, 311.7))) * 43758.5453123);
  }

  float noise(vec2 p) {
    vec2 i = floor(p);
    vec2 f = fract(p);
    vec2 u = f * f * (3.0 - 2.0 * f);
    return mix(
      mix(hash(i), hash(i + vec2(1.0, 0.0)), u.x),
      mix(hash(i + vec2(0.0, 1.0)), hash(i + vec2(1.0, 1.0)), u.x),
      u.y
    );
  }

  float fbm(vec2 p) {
    float value = 0.0;
    float amp = 0.5;
    for (int i = 0; i < 4; i++) {
      value += amp * noise(p);
      p *= 2.03;
      amp *= 0.5;
    }
    return value;
  }

  void main() {
    vec2 p = v_uv * 6.0;
    float t = u_time * 0.012 * u_drift;
    float n1 = fbm(p + vec2(t, -t * 0.6));
    float n2 = fbm(p * 1.7 - vec2(t * 0.4, t));
    float cloud = smoothstep(0.35, 0.85, n1 * 0.65 + n2 * 0.35);

    // Brand hues at whisper level (≤ ~3% luminance): Dusk violet base,
    // Cobalt lift only in the deepest folds — "a place", not a poster
    vec3 dusk = vec3(0.42, 0.067, 0.604) * 0.026;
    vec3 cobalt = vec3(0.369, 0.816, 0.996) * 0.02;
    vec3 color = mix(dusk * 0.35, dusk, cloud) + cobalt * cloud * cloud * 0.5;

    gl_FragColor = vec4(color, 1.0);
  }
`;

interface Props {
  reducedMotion: boolean;
}

export function NebulaBackdrop({ reducedMotion }: Props) {
  const matRef = useRef<ShaderMaterial>(null);

  const uniforms = useMemo(
    () => ({
      u_time: { value: 0 },
      u_drift: { value: reducedMotion ? 0 : 1 },
    }),
    // eslint-disable-next-line react-hooks/exhaustive-deps
    [],
  );

  useFrame(({ clock }) => {
    uniforms.u_time.value = clock.getElapsedTime();
    uniforms.u_drift.value = reducedMotion ? 0 : 1;
  });

  return (
    <mesh position={[0, 0, -420]} renderOrder={-10} frustumCulled={false}>
      <planeGeometry args={[9000, 6000]} />
      <shaderMaterial
        ref={matRef}
        uniforms={uniforms}
        vertexShader={vertexShader}
        fragmentShader={fragmentShader}
        depthWrite={false}
        toneMapped={false}
      />
    </mesh>
  );
}
