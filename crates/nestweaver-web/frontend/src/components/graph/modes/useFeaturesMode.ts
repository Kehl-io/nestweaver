import { useStore } from "../../../stores";
import { useContextGraphMode } from "./useContextGraphMode";

export function useFeaturesMode() {
  const seeds = useStore((s) => s.seeds);
  return useContextGraphMode("features", seeds);
}
