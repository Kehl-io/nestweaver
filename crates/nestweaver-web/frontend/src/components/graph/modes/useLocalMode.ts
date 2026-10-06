import { useStore } from "../../../stores";
import { useContextGraphMode } from "./useContextGraphMode";

export function useLocalMode() {
  const target = useStore((s) => s.inspectedSceneTarget?.mode === "local" &&
    s.inspectedSceneTarget.workspaceId === s.activeWorkspaceId
    ? s.inspectedSceneTarget.uid : s.selectedNodeId);
  return useContextGraphMode("local", target ? [target] : []);
}
