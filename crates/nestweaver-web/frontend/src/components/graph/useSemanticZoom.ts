import { useEffect, useState } from "react";
import { useSigma } from "@react-sigma/core";

// Sigma.js camera ratio: > 1 means zoomed OUT (seeing more), < 1 means zoomed IN (seeing less but bigger).
export type ZoomTier = "overview" | "default" | "detail";

export function useSemanticZoom(): ZoomTier {
  const sigma = useSigma();
  const [tier, setTier] = useState<ZoomTier>("default");

  useEffect(() => {
    const camera = sigma.getCamera();

    const handleUpdate = () => {
      const ratio = camera.getState().ratio;
      if (ratio >= 1.5) {
        setTier("overview");
      } else if (ratio > 0.3) {
        setTier("default");
      } else {
        setTier("detail");
      }
    };

    camera.on("updated", handleUpdate);
    handleUpdate();

    return () => {
      camera.removeListener("updated", handleUpdate);
    };
  }, [sigma]);

  return tier;
}
