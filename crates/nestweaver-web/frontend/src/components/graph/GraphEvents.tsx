import { useEffect } from "react";
import { useSigma, useRegisterEvents } from "@react-sigma/core";
import { useStore } from "../../stores";

export interface ContextMenuState {
  x: number;
  y: number;
  nodeId: string;
}

export function GraphEvents({ onContextMenu }: { onContextMenu: (menu: ContextMenuState | null) => void }) {
  const sigma = useSigma();
  const registerEvents = useRegisterEvents();
  const selectNode = useStore((s) => s.selectNode);
  const hoverNode = useStore((s) => s.hoverNode);
  const setSeeds = useStore((s) => s.setSeeds);

  useEffect(() => {
    registerEvents({
      clickNode: ({ node }) => {
        const attrs = sigma.getGraph().getNodeAttributes(node);
        selectNode(node, attrs.kind || null);
      },
      doubleClickNode: ({ node }) => {
        setSeeds([node]);
      },
      rightClickNode: ({ node, event }) => {
        event.original.preventDefault();
        const e = event.original as MouseEvent;
        onContextMenu({ x: e.clientX, y: e.clientY, nodeId: node });
      },
      enterNode: ({ node }) => {
        hoverNode(node);
        sigma.getContainer().style.cursor = "pointer";
        sigma.refresh({ skipIndexation: true });
      },
      leaveNode: () => {
        hoverNode(null);
        sigma.getContainer().style.cursor = "default";
        sigma.refresh({ skipIndexation: true });
      },
      clickStage: () => {
        selectNode(null);
        onContextMenu(null);
      },
    });
  }, [registerEvents, sigma, selectNode, hoverNode, setSeeds, onContextMenu]);

  return null;
}
