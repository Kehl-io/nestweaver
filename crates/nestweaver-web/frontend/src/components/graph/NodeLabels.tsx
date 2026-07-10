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
//   - candidate placement: each label tries below/above/right/left and takes
//     the first slot clear of every node disc AND every already-placed label;
//     labels with no clean slot are dropped (landmarks/active node fall back
//     to "below") so text is never rendered on top of a node
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
  isHovered,
  isDark,
  onFaded,
}: {
  datum: LabelDatum;
  visible: boolean;
  isHovered: boolean;
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

  const emphasized = datum.isSelected || isHovered;
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
    const membersVisible = zoomBucket <= MEMBER_LABEL_ZOOM;
    // Selection-only: hover is deliberately excluded so moving the mouse never
    // re-runs this O(n^2) label-placement pass (that was the glitchy hover).
    // Hover emphasis is applied cheaply at render time instead.
    const activeNodeId = selectedNodeId;

    graphInstance.forEachNode((uid, attrs) => {
      const idx = buffers.uidToIndex.get(uid);
      if (idx === undefined) return;

      const isSeed = attrs.isSeed === true;
      const forceLabel = attrs.forceLabel === true || isSeed;
      const isSelected = uid === selectedNodeId;
      const isHovered = false; // hover emphasis applied at render, not here

      const isRelatedToActive =
        activeNodeId != null &&
        graphInstance.hasNode(activeNodeId) &&
        (uid === activeNodeId ||
          graphInstance.hasEdge(activeNodeId, uid) ||
          graphInstance.hasEdge(uid, activeNodeId));

      // Landmark hierarchy: hubs always; members only zoomed-in or when
      // they're part of the active neighborhood
      if (!isSelected && !isHovered) {
        if (focusMap && activeNodeId && !isRelatedToActive && !forceLabel)
          return;
        if (!forceLabel && !membersVisible && !isRelatedToActive) return;
      }

      const rawLabel = (attrs.label as string) || uid.split(":").pop() || uid;
      const label = truncateLabel(rawLabel, forceLabel ? 32 : 22);
      const x = buffers.positions[idx * 3];
      const y = buffers.positions[idx * 3 + 1];
      const z = buffers.positions[idx * 3 + 2];

      candidates.push({
        uid,
        label,
        x,
        y,
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

    // Cartographic placement: for each kept label try the four classic
    // anchor positions (below, above, right, left of the node) and take the
    // first that overlaps no node disc and no already-placed label. Labels
    // that can't find a clean slot are dropped unless they're landmarks or
    // the active node — fewer clean labels beat text sitting on nodes.
    const nodeRadius = (i: number) => (buffers.sizes[i] || 6) * 0.45;
    const placedBoxes: Array<{
      x0: number;
      y0: number;
      x1: number;
      y1: number;
    }> = [];
    const boxFor = (
      node: LabelDatum,
      anchor: "below" | "above" | "right" | "left",
    ) => {
      const idx = buffers.uidToIndex.get(node.uid)!;
      const r = nodeRadius(idx) + 3;
      const w = node.label.length * node.fontSize * 0.58;
      const h = node.fontSize * 1.25;
      if (anchor === "below") {
        return {
          x: node.x,
          y: node.y - r - 1,
          x0: node.x - w / 2,
          y0: node.y - r - 1 - h,
          x1: node.x + w / 2,
          y1: node.y - r - 1,
        };
      }
      if (anchor === "above") {
        return {
          x: node.x,
          y: node.y + r + 1 + h,
          x0: node.x - w / 2,
          y0: node.y + r + 1,
          x1: node.x + w / 2,
          y1: node.y + r + 1 + h,
        };
      }
      if (anchor === "right") {
        return {
          x: node.x + r + 2 + w / 2,
          y: node.y + h / 2,
          x0: node.x + r + 2,
          y0: node.y - h / 2,
          x1: node.x + r + 2 + w,
          y1: node.y + h / 2,
        };
      }
      return {
        x: node.x - r - 2 - w / 2,
        y: node.y + h / 2,
        x0: node.x - r - 2 - w,
        y0: node.y - h / 2,
        x1: node.x - r - 2,
        y1: node.y + h / 2,
      };
    };
    const hitsNode = (
      box: { x0: number; y0: number; x1: number; y1: number },
      selfIdx: number,
    ) => {
      for (let i = 0; i < buffers.nodeCount; i++) {
        if (i === selfIdx) continue;
        const nx = buffers.positions[i * 3];
        const ny = buffers.positions[i * 3 + 1];
        const r = nodeRadius(i);
        // circle-vs-box overlap
        const cx = Math.max(box.x0, Math.min(nx, box.x1));
        const cy = Math.max(box.y0, Math.min(ny, box.y1));
        if ((nx - cx) ** 2 + (ny - cy) ** 2 < r * r) return true;
      }
      return false;
    };
    const hitsLabel = (box: {
      x0: number;
      y0: number;
      x1: number;
      y1: number;
    }) =>
      placedBoxes.some(
        (b) => box.x0 < b.x1 && box.x1 > b.x0 && box.y0 < b.y1 && box.y1 > b.y0,
      );

    const out = new Map<string, LabelDatum>();
    // Landmarks place first so detail labels route around them
    const ordered = candidates
      .map((node, index) => ({ node, index }))
      .filter(
        ({ node, index }) =>
          node.isSelected || node.isHovered || kept.has(index),
      )
      .sort((a, b) => Number(b.node.forceLabel) - Number(a.node.forceLabel));

    for (const { node } of ordered) {
      const selfIdx = buffers.uidToIndex.get(node.uid)!;
      const mustPlace = node.isSelected || node.isHovered || node.forceLabel;
      let placed = false;
      for (const anchor of ["below", "above", "right", "left"] as const) {
        const box = boxFor(node, anchor);
        if (!hitsNode(box, selfIdx) && !hitsLabel(box)) {
          out.set(node.uid, { ...node, x: box.x, y: box.y });
          placedBoxes.push(box);
          placed = true;
          break;
        }
      }
      if (!placed && mustPlace) {
        // Landmarks and the active node always show — take "below" even if
        // imperfect rather than hiding the map's names
        const box = boxFor(node, "below");
        out.set(node.uid, { ...node, x: box.x, y: box.y });
        placedBoxes.push(box);
      }
    }
    return out;
  }, [
    buffers,
    graphInstance,
    selectedNodeId,
    focusMap,
    canvasSize.height,
    zoomBucket,
  ]);

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
          isHovered={datum.uid === hoveredNodeId}
          isDark={isDark}
          onFaded={handleFaded}
        />
      ))}
    </>
  );
}
