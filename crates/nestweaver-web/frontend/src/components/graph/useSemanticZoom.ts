import { useEffect, useState } from "react";
import { useSigma } from "@react-sigma/core";

export type ZoomTier = "packages" | "files" | "symbols";

export function useSemanticZoom(): ZoomTier {
  const sigma = useSigma();
  const [tier, setTier] = useState<ZoomTier>("symbols");

  useEffect(() => {
    const camera = sigma.getCamera();

    const handleUpdate = () => {
      const { ratio } = camera.getState();
      if (ratio > 1.5) setTier("packages");
      else if (ratio > 0.5) setTier("files");
      else setTier("symbols");
    };

    camera.on("updated", handleUpdate);
    handleUpdate();

    return () => {
      camera.removeListener("updated", handleUpdate);
    };
  }, [sigma]);

  return tier;
}
