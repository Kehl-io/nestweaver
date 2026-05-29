import { useMemo } from "react";
import { Shape, DoubleSide } from "three";
import louvain from "graphology-communities-louvain";
import { useStore } from "../../../stores";

const COMMUNITY_COLORS = [
  "#E69F00", "#56B4E9", "#009E73", "#F0E442",
  "#0072B2", "#D55E00", "#CC79A7", "#999999",
];

function hexToRgb(hex: string): [number, number, number] {
  const c = hex.slice(1);
  return [
    parseInt(c.slice(0, 2), 16) / 255,
    parseInt(c.slice(2, 4), 16) / 255,
    parseInt(c.slice(4, 6), 16) / 255,
  ];
}

type Point = { x: number; y: number };

function computeConvexHull(points: Point[]): Point[] {
  if (points.length < 3) return points;
  const sorted = [...points].sort((a, b) => a.x - b.x || a.y - b.y);
  const cross = (o: Point, a: Point, b: Point) =>
    (a.x - o.x) * (b.y - o.y) - (a.y - o.y) * (b.x - o.x);

  const lower: Point[] = [];
  for (const p of sorted) {
    while (
      lower.length >= 2 &&
      cross(lower[lower.length - 2], lower[lower.length - 1], p) <= 0
    )
      lower.pop();
    lower.push(p);
  }
  const upper: Point[] = [];
  for (const p of [...sorted].reverse()) {
    while (
      upper.length >= 2 &&
      cross(upper[upper.length - 2], upper[upper.length - 1], p) <= 0
    )
      upper.pop();
    upper.push(p);
  }
  upper.pop();
  lower.pop();
  return lower.concat(upper);
}

function expandHull(hull: Point[], padding: number): Point[] {
  if (hull.length === 0) return hull;
  const cx = hull.reduce((s, p) => s + p.x, 0) / hull.length;
  const cy = hull.reduce((s, p) => s + p.y, 0) / hull.length;
  return hull.map((p) => {
    const dx = p.x - cx;
    const dy = p.y - cy;
    const dist = Math.sqrt(dx * dx + dy * dy) || 1;
    return { x: p.x + (dx / dist) * padding, y: p.y + (dy / dist) * padding };
  });
}

/**
 * Renders Louvain community convex hulls as 3D geometry inside the R3F Canvas.
 * Hulls are placed at z=-0.5 (behind nodes and edges) using Three.js ShapeGeometry.
 * Only rendered when communityOverlay is enabled in the store.
 */
export function CommunityOverlay() {
  const communityOverlay = useStore((s) => s.communityOverlay);
  const graphInstance = useStore((s) => s.graphInstance);
  const graphVersion = useStore((s) => s.graphVersion);

  const hulls = useMemo(() => {
    if (!communityOverlay || !graphInstance || graphInstance.order < 3) return [];

    let communities: Record<string, number>;
    try {
      communities = louvain(graphInstance);
    } catch {
      return [];
    }

    // Group node positions by community id
    const groups = new Map<number, Point[]>();
    graphInstance.forEachNode((node, attrs) => {
      const comm = communities[node];
      if (comm === undefined) return;
      const x = typeof attrs.x === "number" ? attrs.x : 0;
      const y = typeof attrs.y === "number" ? attrs.y : 0;
      if (!groups.has(comm)) groups.set(comm, []);
      groups.get(comm)!.push({ x, y });
    });

    const result: Array<{ points: Point[]; color: string; communityId: number }> = [];
    groups.forEach((points, communityId) => {
      if (points.length < 3) return;
      const hull = computeConvexHull(points);
      const expanded = expandHull(hull, 8);
      if (expanded.length < 3) return;
      result.push({
        points: expanded,
        color: COMMUNITY_COLORS[communityId % COMMUNITY_COLORS.length],
        communityId,
      });
    });

    return result;
  // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [communityOverlay, graphInstance, graphVersion]);

  if (hulls.length === 0) return null;

  return (
    <group>
      {hulls.map((hull) => {
        const shape = new Shape();
        shape.moveTo(hull.points[0].x, hull.points[0].y);
        for (let i = 1; i < hull.points.length; i++) {
          shape.lineTo(hull.points[i].x, hull.points[i].y);
        }
        shape.closePath();

        const [r, g, b] = hexToRgb(hull.color);

        return (
          <mesh key={hull.communityId} position={[0, 0, -0.5]} renderOrder={-2}>
            <shapeGeometry args={[shape]} />
            <meshBasicMaterial
              color={[r, g, b]}
              transparent
              opacity={0.12}
              side={DoubleSide}
              depthTest={false}
              depthWrite={false}
              toneMapped={false}
            />
          </mesh>
        );
      })}
    </group>
  );
}
