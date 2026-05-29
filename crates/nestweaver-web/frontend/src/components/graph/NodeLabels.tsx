import { Html } from "@react-three/drei";
import { useStore } from "../../stores";
import type { GraphBuffers } from "../../hooks/useGraphBridge";

interface Props {
  buffers: GraphBuffers;
}

export function NodeLabels({ buffers }: Props) {
  const graphInstance = useStore((s) => s.graphInstance);
  const selectedNodeId = useStore((s) => s.selectedNodeId);
  const hoveredNodeId = useStore((s) => s.hoveredNodeId);

  if (!graphInstance || buffers.nodeCount === 0) return null;

  // Collect nodes that should show labels
  const labelNodes: Array<{ uid: string; label: string; x: number; y: number; isSeed: boolean }> = [];

  graphInstance.forEachNode((uid, attrs) => {
    const isSeed = attrs.isSeed === true;
    const isSelected = uid === selectedNodeId;
    const isHovered = uid === hoveredNodeId;

    if (isSeed || isSelected || isHovered) {
      const idx = buffers.uidToIndex.get(uid);
      if (idx === undefined) return;

      const label = (attrs.label as string) || uid.split(":").pop() || uid;
      const x = buffers.positions[idx * 3];
      const y = buffers.positions[idx * 3 + 1];
      const size = buffers.sizes[idx] || 6;

      labelNodes.push({ uid, label, x, y: y - size - 2, isSeed });
    }
  });

  return (
    <>
      {labelNodes.map((node) => (
        <Html
          key={node.uid}
          position={[node.x, node.y, 0]}
          center
          style={{
            pointerEvents: "none",
            userSelect: "none",
            whiteSpace: "nowrap",
          }}
          zIndexRange={[100, 0]}
        >
          <div
            style={{
              fontSize: "11px",
              fontFamily: "system-ui, sans-serif",
              fontWeight: node.uid === selectedNodeId ? 600 : 400,
              color: "var(--color-text, #e2e8f0)",
              textShadow: "0 1px 3px rgba(0,0,0,0.8), 0 0px 6px rgba(0,0,0,0.5)",
              padding: "1px 4px",
              borderRadius: "3px",
              background: node.uid === selectedNodeId ? "rgba(59, 130, 246, 0.2)" : "transparent",
            }}
          >
            {node.label}
          </div>
        </Html>
      ))}
    </>
  );
}
