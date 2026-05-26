import { useEffect, useRef } from "react";
import { useSigma } from "@react-sigma/core";

export function GraphMinimap() {
  const sigma = useSigma();
  const canvasRef = useRef<HTMLCanvasElement>(null);

  useEffect(() => {
    const canvas = canvasRef.current;
    if (!canvas) return;
    const ctx = canvas.getContext("2d");
    if (!ctx) return;

    const draw = () => {
      const graph = sigma.getGraph();
      const W = canvas.width;
      const H = canvas.height;
      ctx.clearRect(0, 0, W, H);

      // Background
      ctx.fillStyle = "rgba(255,255,255,0.9)";
      ctx.fillRect(0, 0, W, H);
      ctx.strokeStyle = "#e5e7eb";
      ctx.strokeRect(0, 0, W, H);

      if (graph.order === 0) return;

      // Find bounds
      let minX = Infinity,
        maxX = -Infinity,
        minY = Infinity,
        maxY = -Infinity;
      graph.forEachNode((_node: string, attrs: Record<string, any>) => {
        const x = attrs.x as number;
        const y = attrs.y as number;
        if (x < minX) minX = x;
        if (x > maxX) maxX = x;
        if (y < minY) minY = y;
        if (y > maxY) maxY = y;
      });

      const rangeX = maxX - minX || 1;
      const rangeY = maxY - minY || 1;
      const pad = 10;
      const scale = Math.min(
        (W - pad * 2) / rangeX,
        (H - pad * 2) / rangeY,
      );

      // Draw nodes as dots
      graph.forEachNode((_node: string, attrs: Record<string, any>) => {
        const px = pad + ((attrs.x as number) - minX) * scale;
        const py = pad + ((attrs.y as number) - minY) * scale;
        ctx.fillStyle = (attrs.color as string) || "#999";
        ctx.beginPath();
        ctx.arc(px, py, 2, 0, Math.PI * 2);
        ctx.fill();
      });
    };

    draw();
    const interval = setInterval(draw, 500);
    return () => clearInterval(interval);
  }, [sigma]);

  return (
    <canvas
      ref={canvasRef}
      width={160}
      height={120}
      className="rounded border border-[var(--color-border)] shadow-sm"
    />
  );
}
