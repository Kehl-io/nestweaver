import { useStore } from "../../stores";

export type ZoomTier = "overview" | "default" | "detail" | "packages" | "files";

/**
 * Returns the current semantic zoom tier derived from the camera's Z position,
 * which is written into the zustand store by CameraZoomBridge (inside the Canvas).
 *
 * Thresholds assume a perspective camera starting at z=500:
 *   z > 700  → "overview"
 *   z < 200  → "detail"
 *   otherwise → "default"
 */
export function useSemanticZoom(): ZoomTier {
  const cameraZoom = useStore((s) => s.cameraZoom);

  if (cameraZoom > 700) return "overview";
  if (cameraZoom < 200) return "detail";
  return "default";
}
