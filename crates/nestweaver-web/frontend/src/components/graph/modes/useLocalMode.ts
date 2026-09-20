import { useStore } from "../../../stores";
import { useContextGraphMode } from "./useContextGraphMode";

export function useLocalMode() {
  const selectedNodeId = useStore((s) => s.selectedNodeId);
  return useContextGraphMode("local", selectedNodeId ? [selectedNodeId] : []);
}
