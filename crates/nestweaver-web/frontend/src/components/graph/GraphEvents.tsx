// Phase 1: Sigma event handlers removed during Sigma -> R3F migration.
// Click/hover/right-click events are now handled directly in GraphCanvas
// via R3F pointer events and useGPUPicking.

export interface ContextMenuState {
  x: number;
  y: number;
  nodeId: string;
}

export function GraphEvents(_props: { onContextMenu: (menu: ContextMenuState | null) => void }) {
  return null;
}
