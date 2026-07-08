import { useMemo } from "react";
import { useThree } from "@react-three/fiber";
import { Html } from "@react-three/drei";
import { useStore } from "../../stores";
import type { GraphBuffers } from "../../hooks/useGraphBridge";

interface Props {
  buffers: GraphBuffers;
}

// Constant-density label grid (sigma.js pattern, verified): the viewport is
// divided into fixed-pixel cells and each cell shows only its worthiest
// label — label count scales with screen area, not node count, so density
// stays readable from 20 nodes to 10k.
const LABEL_CELL_PX = 100;

function truncateLabel(name: string, max = 20): string {
  return name.length > max ? name.slice(0, max) + "…" : name;
}

export function NodeLabels({ buffers }: Props) {
  const graphInstance = useStore((s) => s.graphInstance);
  const selectedNodeId = useStore((s) => s.selectedNodeId);
  const hoveredNodeId = useStore((s) => s.hoveredNodeId);
  const cameraZoom = useStore((s) => s.cameraZoom);
  const layoutMode = useStore((s) => s.layoutMode);
  const canvasSize = useThree((s) => s.size);
  const focusMap = layoutMode === "zen";

  // Bucket the zoom so the grid recomputes ~once per 25 units of dolly
  // instead of on every 0.5-unit camera tick (the grid pass is O(nodes)
  // plus Html portal reconciliation — unbounded recompute janks at scale)
  const zoomBucket = Math.round(cameraZoom / 25) * 25;

  const visibleLabels = useMemo(() => {
  if (!graphInstance || buffers.nodeCount === 0) return [];

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

    // Focus-map narrows candidates to the active neighborhood; the screen-
    // space grid below handles density at every zoom level
    if (!isSelected && !isHovered) {
      if (focusMap && activeNodeId && !isRelatedToActive && !forceLabel) return;
    }

    const rawLabel = (attrs.label as string) || uid.split(":").pop() || uid;
    // Force-labeled nodes (repo hubs, seeds) get more room — repo names like
    // "bx-react-native-client" must not truncate on the landing scene
    const label = truncateLabel(rawLabel, forceLabel ? 32 : 20);
    const x = buffers.positions[idx * 3];
    const y = buffers.positions[idx * 3 + 1];
    const radialX = x - centerX;
    const radialY = y - centerY;
    const radialLength = Math.hypot(radialX, radialY);
    // High-degree hubs sit inside a satellite ring; push their label past it
    const ringClearance = Math.min(graphInstance.degree(uid) * 1.4, 34);
    const labelOffset = size * 0.95 + 8 + ringClearance;
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
        : y - size - 2 - ringClearance;

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

  // Constant-density selection: bin candidates into fixed-pixel screen
  // cells and keep the worthiest label per cell (selected > hovered >
  // force-labeled > size). World→screen scale for the perspective camera:
  // px-per-world-unit ≈ viewportHeight / (2·tan(fov/2)·cameraZ), fov 50°.
  const pxPerWorld =
    canvasSize.height / (2 * Math.tan((50 * Math.PI) / 360) * Math.max(zoomBucket, 1));
  const cellBest = new Map<string, { score: number; index: number }>();
  labelNodes.forEach((node, index) => {
    const score = node.isSelected
      ? 4e12
      : node.isHovered
        ? 2e12
        : (node.forceLabel ? 1e6 : 0) + node.size;
    const cx = Math.floor((node.x * pxPerWorld) / LABEL_CELL_PX);
    const cy = Math.floor((node.y * pxPerWorld) / LABEL_CELL_PX);
    const key = `${cx}:${cy}`;
    const best = cellBest.get(key);
    if (!best || score > best.score) cellBest.set(key, { score, index });
  });
  const keptIndices = new Set(
    [...cellBest.values()].map((entry) => entry.index),
  );
  return labelNodes.filter(
    (node, index) =>
      node.isSelected || node.isHovered || keptIndices.has(index),
  );
  // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [buffers, graphInstance, selectedNodeId, hoveredNodeId, focusMap, canvasSize.height, zoomBucket]);

  if (!graphInstance || buffers.nodeCount === 0) return null;

  // Font size scales inversely with zoom: bigger when close, smaller when far
  const baseFontSize = zoomBucket < 300 ? 12 : zoomBucket < 600 ? 11 : 10;

  return (
    <>
      {visibleLabels.map((node) => {
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
