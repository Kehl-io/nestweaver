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
  const layoutMode = useStore((s) => s.layoutMode);
  const focusMap = layoutMode === "zen";

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
    forceLabel: boolean;
    isSelected: boolean;
    isHovered: boolean;
  }> = [];
  let centerX = 0;
  let centerY = 0;
  for (let i = 0; i < buffers.nodeCount; i++) {
    centerX += buffers.positions[i * 3];
    centerY += buffers.positions[i * 3 + 1];
  }
  centerX /= buffers.nodeCount;
  centerY /= buffers.nodeCount;

  graphInstance.forEachNode((uid, attrs) => {
    const idx = buffers.uidToIndex.get(uid);
    if (idx === undefined) return;

    const isSeed = attrs.isSeed === true;
    const forceLabel = attrs.forceLabel === true || isSeed;
    const isSelected = uid === selectedNodeId;
    const isHovered = uid === hoveredNodeId;
    const size = buffers.sizes[idx] || 6;

    const activeNodeId = hoveredNodeId ?? selectedNodeId;
    const isRelatedToActive =
      activeNodeId != null &&
      graphInstance.hasNode(activeNodeId) &&
      graphInstance.hasNode(uid) &&
      (uid === activeNodeId || graphInstance.hasEdge(activeNodeId, uid) || graphInstance.hasEdge(uid, activeNodeId));

    // Zoom-aware visibility:
    // - Zoomed in (z < 300): show all labels
    // - Default zoom (300-600): show labels for nodes with size > median
    // - Zoomed out (z > 600): show only seed labels
    // Selected/hovered are always visible regardless of zoom
    if (!isSelected && !isHovered) {
      if (focusMap && activeNodeId && !isRelatedToActive && !forceLabel) return;
      if (focusMap && !activeNodeId && !forceLabel && size <= medianSize) return;
      if (cameraZoom > 600 && !forceLabel) return;
      if (cameraZoom >= 300 && cameraZoom <= 600 && size <= medianSize && !forceLabel) return;
    }

    const rawLabel = (attrs.label as string) || uid.split(":").pop() || uid;
    const label = truncateLabel(rawLabel);
    const x = buffers.positions[idx * 3];
    const y = buffers.positions[idx * 3 + 1];
    const radialX = x - centerX;
    const radialY = y - centerY;
    const radialLength = Math.hypot(radialX, radialY);
    const labelOffset = size * 0.95 + 8;
    const horizontalSpoke =
      radialLength > 1 && Math.abs(radialX) > Math.abs(radialY) * 1.25;
    const verticalDirection =
      Math.abs(radialY) > 1 ? Math.sign(radialY) : -1;
    const labelX =
      radialLength > 1 && !horizontalSpoke
        ? x + (radialX / radialLength) * labelOffset
        : x;
    const labelY =
      radialLength > 1
        ? horizontalSpoke
          ? y + verticalDirection * labelOffset
          : y + (radialY / radialLength) * labelOffset
        : y - size - 2;

    labelNodes.push({
      uid,
      label,
      x: labelX,
      y: labelY,
      size,
      isSeed,
      forceLabel,
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
            style={{
              pointerEvents: "none",
              userSelect: "none",
              whiteSpace: "nowrap",
            }}
            zIndexRange={[18, 0]}
          >
            <div
              className={`graph-node-label ${
                node.isSelected
                  ? "graph-node-label-selected"
                  : node.isHovered
                    ? "graph-node-label-hovered"
                    : ""
              } ${focusMap ? "graph-node-label-focus" : ""}`}
              style={{
                fontSize: `${baseFontSize}px`,
                fontWeight: isHighlighted ? 600 : 500,
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
