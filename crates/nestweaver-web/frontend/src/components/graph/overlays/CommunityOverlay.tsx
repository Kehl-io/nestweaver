import { useEffect, useRef, useCallback } from "react";
import { useSigma } from "@react-sigma/core";
import louvain from "graphology-communities-louvain";
import { useStore } from "../../../stores";

const COMMUNITY_COLORS = [
  "#3B82F6",
  "#EF4444",
  "#22C55E",
  "#F59E0B",
  "#8B5CF6",
  "#EC4899",
  "#06B6D4",
  "#F97316",
  "#14B8A6",
  "#6366F1",
];

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

  const cx = hull.reduce((sum, p) => sum + p.x, 0) / hull.length;
  const cy = hull.reduce((sum, p) => sum + p.y, 0) / hull.length;

  return hull.map((p) => {
    const dx = p.x - cx;
    const dy = p.y - cy;
    const dist = Math.sqrt(dx * dx + dy * dy);
    if (dist === 0) return { x: p.x + padding, y: p.y };
    return {
      x: p.x + (dx / dist) * padding,
      y: p.y + (dy / dist) * padding,
    };
  });
}

export function CommunityOverlay() {
  const sigma = useSigma();
  const canvasRef = useRef<HTMLCanvasElement | null>(null);
  const communityOverlay = useStore((s) => s.communityOverlay);

  const draw = useCallback(() => {
    const canvas = canvasRef.current;
    if (!canvas) return;

    const container = sigma.getContainer();
    const width = container.offsetWidth;
    const height = container.offsetHeight;

    if (canvas.width !== width || canvas.height !== height) {
      canvas.width = width;
      canvas.height = height;
    }

    const ctx = canvas.getContext("2d");
    if (!ctx) return;

    ctx.clearRect(0, 0, width, height);

    const graph = sigma.getGraph();
    if (graph.order === 0) return;

    // Run Louvain community detection
    let communities: Record<string, number>;
    try {
      communities = louvain(graph);
    } catch {
      return;
    }

    // Group nodes by community
    const groups = new Map<number, Point[]>();
    graph.forEachNode((node) => {
      const community = communities[node];
      if (community === undefined) return;

      const displayData = sigma.getNodeDisplayData(node);
      if (!displayData) return;

      if (!groups.has(community)) {
        groups.set(community, []);
      }
      groups.get(community)!.push({ x: displayData.x, y: displayData.y });
    });

    // Draw convex hull for each community
    groups.forEach((points, community) => {
      if (points.length < 2) return;

      const color = COMMUNITY_COLORS[community % COMMUNITY_COLORS.length];
      const hull = computeConvexHull(points);
      const expanded = expandHull(hull, 20);

      if (expanded.length < 2) return;

      ctx.beginPath();
      ctx.moveTo(expanded[0].x, expanded[0].y);
      for (let i = 1; i < expanded.length; i++) {
        ctx.lineTo(expanded[i].x, expanded[i].y);
      }
      ctx.closePath();

      ctx.fillStyle = color + "20";
      ctx.fill();

      ctx.strokeStyle = color + "60";
      ctx.lineWidth = 1.5;
      ctx.stroke();
    });
  }, [sigma]);

  useEffect(() => {
    if (!communityOverlay) return;

    // Initial draw
    draw();

    // Redraw periodically to stay aligned during layout animation
    const interval = setInterval(draw, 500);

    return () => {
      clearInterval(interval);
      // Clear canvas on cleanup
      const canvas = canvasRef.current;
      if (canvas) {
        const ctx = canvas.getContext("2d");
        if (ctx) ctx.clearRect(0, 0, canvas.width, canvas.height);
      }
    };
  }, [communityOverlay, draw]);

  if (!communityOverlay) return null;

  return (
    <canvas
      ref={canvasRef}
      style={{
        position: "absolute",
        inset: 0,
        pointerEvents: "none",
        zIndex: 5,
      }}
    />
  );
}
