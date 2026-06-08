import { useMemo } from "react";
import { Html } from "@react-three/drei";
import { useStore } from "../../stores";
import type { GraphBuffers } from "../../hooks/useGraphBridge";

interface Props {
  buffers: GraphBuffers;
}

function truncateLabel(name: string, max = 20): string {
  return name.length > max ? name.slice(0, max) + "…" : name;
}

export function NodeLabels({ buffers }: Props) {
  const graphInstance = useStore((s) => s.graphInstance);
  const selectedNodeId = useStore((s) => s.selectedNodeId);
  const hoveredNodeId = useStore((s) => s.hoveredNodeId);
  const cameraZoom = useStore((s) => s.cameraZoom);

  // Compute median size for filtering at medium zoom
  const medianSize = useMemo(() => {
    if (buffers.nodeCount === 0) return 6;
    const sorted = Array.from(buffers.sizes).sort((a, b) => a - b);
    const mid = Math.floor(sorted.length / 2);
    return sorted.length % 2 === 0
      ? (sorted[mid - 1] + sorted[mid]) / 2
      : sorted[mid];
  }, [buffers.sizes, buffers.nodeCount]);

  if (!graphInstance || buffers.nodeCount === 0) return null;

  // Collect all nodes with label data
  const labelNodes: Array<{
    uid: string;
    label: string;
    x: number;
    y: number;
    size: number;
    isSeed: boolean;
    isSelected: boolean;
    isHovered: boolean;
  }> = [];

  graphInstance.forEachNode((uid, attrs) => {
    const idx = buffers.uidToIndex.get(uid);
    if (idx === undefined) return;

    const isSeed = attrs.isSeed === true;
    const isSelected = uid === selectedNodeId;
    const isHovered = uid === hoveredNodeId;
    const size = buffers.sizes[idx] || 6;

    // Zoom-aware visibility:
    // - Zoomed in (z < 300): show all labels
    // - Default zoom (300-600): show labels for nodes with size > median
    // - Zoomed out (z > 600): show only seed labels
    // Selected/hovered are always visible regardless of zoom
    if (!isSelected && !isHovered) {
      if (cameraZoom > 600 && !isSeed) return;
      if (cameraZoom >= 300 && cameraZoom <= 600 && size <= medianSize && !isSeed) return;
    }

    const rawLabel = (attrs.label as string) || uid.split(":").pop() || uid;
    const label = truncateLabel(rawLabel);
    const x = buffers.positions[idx * 3];
    const y = buffers.positions[idx * 3 + 1];

    labelNodes.push({
      uid,
      label,
      x,
      y: y - size - 2,
      size,
      isSeed,
      isSelected,
      isHovered,
    });
  });

  // Font size scales inversely with zoom: bigger when close, smaller when far
  const baseFontSize = cameraZoom < 300 ? 12 : cameraZoom < 600 ? 11 : 10;

  return (
    <>
      {labelNodes.map((node) => {
        const isHighlighted = node.isSelected || node.isHovered;
        return (
          <Html
            key={node.uid}
            position={[node.x, node.y, 0]}
            center
            occlude
            style={{
              pointerEvents: "none",
              userSelect: "none",
              whiteSpace: "nowrap",
            }}
            zIndexRange={[100, 0]}
          >
            <div
              className={`graph-node-label ${
                node.isSelected
                  ? "graph-node-label-selected"
                  : node.isHovered
                    ? "graph-node-label-hovered"
                    : ""
              }`}
              style={{
                fontSize: `${baseFontSize}px`,
                fontWeight: isHighlighted ? 700 : 560,
              }}
            >
              {node.label}
            </div>
          </Html>
        );
      })}
    </>
  );
}
