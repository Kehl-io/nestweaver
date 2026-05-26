import { useEffect, useRef, useCallback, useState } from "react";
import { useSigma } from "@react-sigma/core";
import louvain from "graphology-communities-louvain";
import { useStore } from "../../../stores";

// Okabe-Ito colorblind-safe palette
const COMMUNITY_COLORS = [
  "#E69F00",
  "#56B4E9",
  "#009E73",
  "#F0E442",
  "#0072B2",
  "#D55E00",
  "#CC79A7",
  "#999999",
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

function useIsDark(): boolean {
  const theme = useStore((s) => s.theme);
  const [isDark, setIsDark] = useState(() => {
    if (theme === "dark") return true;
    if (theme === "light") return false;
    return window.matchMedia("(prefers-color-scheme: dark)").matches;
  });

  useEffect(() => {
    if (theme === "dark") {
      setIsDark(true);
      return;
    }
    if (theme === "light") {
      setIsDark(false);
      return;
    }
    const mq = window.matchMedia("(prefers-color-scheme: dark)");
    setIsDark(mq.matches);
    const handler = (e: MediaQueryListEvent) => setIsDark(e.matches);
    mq.addEventListener("change", handler);
    return () => mq.removeEventListener("change", handler);
  }, [theme]);

  return isDark;
}

export function CommunityOverlay() {
  const sigma = useSigma();
  const canvasRef = useRef<HTMLCanvasElement | null>(null);
  const communityOverlay = useStore((s) => s.communityOverlay);
  const isDark = useIsDark();

  // Memoize Louvain results — only recompute when graph topology changes
  const communitiesRef = useRef<Record<string, number> | null>(null);
  const lastOrderRef = useRef<number>(-1);

  const draw = useCallback(() => {
    const canvas = canvasRef.current;
    if (!canvas) return;

    const container = sigma.getContainer();
    const width = container.clientWidth;
    const height = container.clientHeight;

    if (canvas.width !== width || canvas.height !== height) {
      canvas.width = width;
      canvas.height = height;
    }

    const ctx = canvas.getContext("2d");
    if (!ctx) return;

    ctx.clearRect(0, 0, width, height);

    const graph = sigma.getGraph();
    if (graph.order === 0) return;

    // Re-run Louvain only when node count changes (topology change)
    if (communitiesRef.current === null || lastOrderRef.current !== graph.order) {
      try {
        communitiesRef.current = louvain(graph);
        lastOrderRef.current = graph.order;
      } catch {
        return;
      }
    }

    const communities = communitiesRef.current;

    // Group nodes by community — reproject graph-space coords to screen-space each frame
    const groups = new Map<number, Point[]>();
    graph.forEachNode((node) => {
      const community = communities[node];
      if (community === undefined) return;

      const displayData = sigma.getNodeDisplayData(node);
      if (!displayData) return;

      // getNodeDisplayData returns graph-space coordinates; project to viewport pixels
      const screenPos = sigma.graphToViewport({ x: displayData.x, y: displayData.y });

      if (!groups.has(community)) {
        groups.set(community, []);
      }
      groups.get(community)!.push({ x: screenPos.x, y: screenPos.y });
    });

    // Theme-aware opacity
    const fillAlpha = isDark ? 0.15 : 0.08;
    const strokeAlpha = 0.4;

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

      ctx.globalAlpha = fillAlpha;
      ctx.fillStyle = color;
      ctx.fill();

      ctx.globalAlpha = strokeAlpha;
      ctx.strokeStyle = color;
      ctx.lineWidth = 1.5;
      ctx.stroke();

      ctx.globalAlpha = 1;
    });
  }, [sigma, isDark]);

  useEffect(() => {
    if (!communityOverlay) return;

    // Reset memoized communities when overlay is toggled on
    communitiesRef.current = null;
    lastOrderRef.current = -1;

    // Initial draw
    draw();

    // Redraw on every Sigma render (follows pan/zoom) — Louvain is skipped unless topology changed
    sigma.on("afterRender", draw);

    return () => {
      sigma.off("afterRender", draw);
      // Clear canvas on cleanup
      const canvas = canvasRef.current;
      if (canvas) {
        const ctx = canvas.getContext("2d");
        if (ctx) ctx.clearRect(0, 0, canvas.width, canvas.height);
      }
    };
  }, [communityOverlay, sigma, draw]);

  if (!communityOverlay) return null;

  return (
    <canvas
      ref={canvasRef}
      style={{
        position: "absolute",
        inset: 0,
        width: "100%",
        height: "100%",
        pointerEvents: "none",
        zIndex: 10,
      }}
    />
  );
}
