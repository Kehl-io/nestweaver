import { useEffect, useRef } from "react";
import { useStore } from "../stores";
import type { GraphMode } from "../api/types";

export function useDeepLink() {
  const seeds = useStore((s) => s.seeds);
  const graphMode = useStore((s) => s.graphMode);
  const setSeeds = useStore((s) => s.setSeeds);
  const setGraphMode = useStore((s) => s.setGraphMode);
  const initializedRef = useRef(false);

  // On mount: read URL params and apply to store
  useEffect(() => {
    if (initializedRef.current) return;
    initializedRef.current = true;

    const params = new URLSearchParams(window.location.search);
    const seedParam = params.get("seeds");
    const modeParam = params.get("mode");

    if (seedParam) {
      setSeeds(seedParam.split(",").filter(Boolean));
    }
    if (modeParam) {
      setGraphMode(modeParam as GraphMode);
    }
  }, [setSeeds, setGraphMode]);

  // On state change: update URL
  useEffect(() => {
    if (!initializedRef.current) return;

    const params = new URLSearchParams();
    if (seeds.length > 0) params.set("seeds", seeds.join(","));
    if (graphMode !== "overview") params.set("mode", graphMode);

    const url = params.toString()
      ? `${window.location.pathname}?${params}`
      : window.location.pathname;

    window.history.replaceState(null, "", url);
  }, [seeds, graphMode]);
}
