import { useEffect, useMemo, useRef, useState } from "react";
import { useFrame, useThree } from "@react-three/fiber";
import { Text } from "@react-three/drei";
import type { Mesh, MeshBasicMaterial } from "three";
import { useStore } from "../../stores";
import type { GraphBuffers } from "../../hooks/useGraphBridge";

// Labels are SDF text rendered *inside* the WebGL scene (troika via drei
// <Text>) instead of DOM overlays — they move pixel-locked with the camera,
// scale naturally with zoom, and can fade. Policy (maps/sigma pattern):
//   - hubs (forceLabel/seeds) are landmarks: always labeled
//   - member labels appear when zoomed in (or hovered/selected/related)
//   - a constant-density screen grid caps how many labels show per area
//   - every enter/exit is a fade, never a pop
const LABEL_CELL_PX = 100;
const MEMBER_LABEL_ZOOM = 420; // members labeled when camera z is closer than this
const LABEL_FONT = "/fonts/inter-500.ttf";

function truncateLabel(name: string, max = 20): string {
  return name.length > max ? name.slice(0, max) + "…" : name;
}

interface LabelDatum {
  uid: string;
  label: string;
  x: number;
  y: number;
  z: number;
  fontSize: number;
  isSelected: boolean;
  isHovered: boolean;
  forceLabel: boolean;
}

function FadingLabel({
  datum,
  visible,
  isDark,
  onFaded,
}: {
  datum: LabelDatum;
  visible: boolean;
  isDark: boolean;
  onFaded: (uid: string) => void;
}) {
  const ref = useRef<Mesh & { material: MeshBasicMaterial }>(null);
  const opacityRef = useRef(0);

  useFrame((_, delta) => {
    const mesh = ref.current;
    if (!mesh) return;
    const target = visible ? 1 : 0;
    const current = opacityRef.current;
    const next = current + (target - current) * Math.min(1, delta * 9);
    opacityRef.current = next;
    if (mesh.material) {
      mesh.material.opacity = next;
      mesh.material.transparent = true;
    }
    if (!visible && next < 0.02) onFaded(datum.uid);
  });

  const emphasized = datum.isSelected || datum.isHovered;
  const fill = isDark
    ? emphasized
      ? "#f2f6fb"
      : "#b7c0cf"
    : emphasized
      ? "#111826"
      : "#3d4656";
  const outline = isDark ? "#080b11" : "#eef3f8";

  return (
    <Text
      ref={ref}
      font={LABEL_FONT}
      position={[datum.x, datum.y, datum.z + 2]}
      fontSize={datum.fontSize * (emphasized ? 1.12 : 1)}
      color={fill}
      anchorX="center"
      anchorY="top"
      outlineWidth={datum.fontSize * 0.14}
      outlineColor={outline}
      outlineOpacity={0.85}
      renderOrder={20}
      material-depthTest={false}
      material-toneMapped={false}
    >
      {datum.label}
    </Text>
  );
}

interface Props {
  buffers: GraphBuffers;
}

export function NodeLabels({ buffers }: Props) {
  const graphInstance = useStore((s) => s.graphInstance);
  const selectedNodeId = useStore((s) => s.selectedNodeId);
  const hoveredNodeId = useStore((s) => s.hoveredNodeId);
  const cameraZoom = useStore((s) => s.cameraZoom);
  const layoutMode = useStore((s) => s.layoutMode);
  const theme = useStore((s) => s.theme);
  const canvasSize = useThree((s) => s.size);
  const focusMap = layoutMode === "zen";
  const isDark =
    theme === "dark" ||
    (theme === "system" &&
      typeof window !== "undefined" &&
      window.matchMedia("(prefers-color-scheme: dark)").matches);

  // Bucket zoom so selection recomputes ~once per 25 units of dolly, not on
  // every 0.5-unit camera tick (fades bridge the discrete steps visually)
  const zoomBucket = Math.round(cameraZoom / 25) * 25;

  const visibleLabels = useMemo(() => {
    if (!graphInstance || buffers.nodeCount === 0) {
      return new Map<string, LabelDatum>();
    }

    const candidates: LabelDatum[] = [];
    let centerX = 0;
    let centerY = 0;
    for (let i = 0; i < buffers.nodeCount; i++) {
      centerX += buffers.positions[i * 3];
      centerY += buffers.positions[i * 3 + 1];
    }
    centerX /= buffers.nodeCount;
    centerY /= buffers.nodeCount;

    const membersVisible = zoomBucket <= MEMBER_LABEL_ZOOM;
    const activeNodeId = hoveredNodeId ?? selectedNodeId;

    graphInstance.forEachNode((uid, attrs) => {
      const idx = buffers.uidToIndex.get(uid);
      if (idx === undefined) return;

      const isSeed = attrs.isSeed === true;
      const forceLabel = attrs.forceLabel === true || isSeed;
      const isSelected = uid === selectedNodeId;
      const isHovered = uid === hoveredNodeId;
      const size = buffers.sizes[idx] || 6;

      const isRelatedToActive =
        activeNodeId != null &&
        graphInstance.hasNode(activeNodeId) &&
        (uid === activeNodeId ||
          graphInstance.hasEdge(activeNodeId, uid) ||
          graphInstance.hasEdge(uid, activeNodeId));

      // Landmark hierarchy: hubs always; members only zoomed-in or when
      // they're part of the active neighborhood
      if (!isSelected && !isHovered) {
        if (focusMap && activeNodeId && !isRelatedToActive && !forceLabel) return;
        if (!forceLabel && !membersVisible && !isRelatedToActive) return;
      }

      const rawLabel = (attrs.label as string) || uid.split(":").pop() || uid;
      const label = truncateLabel(rawLabel, forceLabel ? 32 : 22);
      const x = buffers.positions[idx * 3];
      const y = buffers.positions[idx * 3 + 1];
      const z = buffers.positions[idx * 3 + 2];
      const radialX = x - centerX;
      const radialY = y - centerY;
      const radialLength = Math.hypot(radialX, radialY);
      const ringClearance = Math.min(graphInstance.degree(uid) * 1.4, 34);
      const labelOffset = size * 0.62 + 5 + ringClearance;
      const labelX =
        radialLength > 1 ? x + (radialX / radialLength) * labelOffset * 0.35 : x;
      const labelY =
        radialLength > 1
          ? y - Math.abs(labelOffset) * 0.85
          : y - size * 0.62 - 4 - ringClearance;

      candidates.push({
        uid,
        label,
        x: labelX,
        y: labelY,
        z,
        fontSize: forceLabel ? 7 : 4.8,
        isSelected,
        isHovered,
        forceLabel,
      });
    });

    // Constant-density screen grid: worthiest label per fixed-pixel cell
    const pxPerWorld =
      canvasSize.height /
      (2 * Math.tan((50 * Math.PI) / 360) * Math.max(zoomBucket, 1));
    const cellBest = new Map<string, { score: number; index: number }>();
    candidates.forEach((node, index) => {
      const score = node.isSelected
        ? 4e12
        : node.isHovered
          ? 2e12
          : (node.forceLabel ? 1e6 : 0) + node.fontSize;
      const cx = Math.floor((node.x * pxPerWorld) / LABEL_CELL_PX);
      const cy = Math.floor((node.y * pxPerWorld) / LABEL_CELL_PX);
      const key = `${cx}:${cy}`;
      const best = cellBest.get(key);
      if (!best || score > best.score) cellBest.set(key, { score, index });
    });
    const kept = new Set([...cellBest.values()].map((entry) => entry.index));

    const out = new Map<string, LabelDatum>();
    candidates.forEach((node, index) => {
      if (node.isSelected || node.isHovered || kept.has(index)) {
        out.set(node.uid, node);
      }
    });
    return out;
  // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [buffers, graphInstance, selectedNodeId, hoveredNodeId, focusMap, canvasSize.height, zoomBucket]);

  // Fade lifecycle: render the union of currently-visible labels and those
  // still fading out; FadingLabel reports back when it reaches zero
  const [retained, setRetained] = useState<Map<string, LabelDatum>>(new Map());
  useEffect(() => {
    setRetained((previous) => {
      const next = new Map(previous);
      visibleLabels.forEach((datum, uid) => next.set(uid, datum));
      return next;
    });
  }, [visibleLabels]);

  const handleFaded = useRef((uid: string) => {
    setRetained((previous) => {
      if (!previous.has(uid)) return previous;
      const next = new Map(previous);
      next.delete(uid);
      return next;
    });
  }).current;

  if (!graphInstance || buffers.nodeCount === 0) return null;

  return (
    <>
      {[...retained.values()].map((datum) => (
        <FadingLabel
          key={datum.uid}
          datum={datum}
          visible={visibleLabels.has(datum.uid)}
          isDark={isDark}
          onFaded={handleFaded}
        />
      ))}
    </>
  );
}
