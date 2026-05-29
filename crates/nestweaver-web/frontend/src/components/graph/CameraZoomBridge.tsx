import { useFrame } from "@react-three/fiber";
import { useRef } from "react";
import { useStore } from "../../stores";

/**
 * Runs inside the R3F Canvas and writes the camera's z-position to the zustand
 * store so components outside the Canvas (e.g. useSemanticZoom) can read it.
 *
 * With a perspective camera + OrbitControls (rotate disabled), dollying in/out
 * moves the camera along the Z axis. Initial position is z=500.
 *
 * Thresholds:
 *   z > 700  → "overview"  (far out)
 *   200–700  → "default"
 *   z < 200  → "detail"   (zoomed in)
 */
export function CameraZoomBridge() {
  const setCameraZoom = useStore((s) => s.setCameraZoom);
  const lastZoomRef = useRef(500);

  useFrame((state) => {
    const z = state.camera.position.z;
    if (Math.abs(z - lastZoomRef.current) > 0.5) {
      lastZoomRef.current = z;
      setCameraZoom(z);
    }
  });

  return null;
}
