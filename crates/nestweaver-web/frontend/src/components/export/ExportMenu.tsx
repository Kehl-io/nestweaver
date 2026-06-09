import { useState } from "react";
import { useStore } from "../../stores";

interface Props {
  onClose: () => void;
}

interface ExportNode {
  uid: string;
  x: number;
  y: number;
  size: number;
  color: string;
  label: string;
}

interface ExportEdge {
  source: string;
  target: string;
  color: string;
  thickness: number;
}

interface ExportSnapshot {
  nodes: ExportNode[];
  edges: ExportEdge[];
  width: number;
  height: number;
  background: string;
  legend: boolean;
}

function numberAttribute(value: unknown, fallback: number): number {
  return typeof value === "number" && Number.isFinite(value) ? value : fallback;
}

function stringAttribute(value: unknown, fallback: string): string {
  return typeof value === "string" && value.trim().length > 0 ? value : fallback;
}

function downloadBlob(blob: Blob, filename: string) {
  const url = URL.createObjectURL(blob);
  const a = document.createElement("a");
  a.href = url;
  a.download = filename;
  document.body.appendChild(a);
  a.click();
  a.remove();
  window.setTimeout(() => URL.revokeObjectURL(url), 1000);
}

export function ExportMenu({ onClose }: Props) {
  const [exporting, setExporting] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const graph = useStore((s) => s.graphInstance);

  const buildSnapshot = (): ExportSnapshot | null => {
    if (!graph || graph.order === 0) return null;

    const width = 1920;
    const height = 1080;
    const padding = 96;
    const nodes = graph.nodes();
    const positioned = nodes.map((uid) => {
      const x = numberAttribute(graph.getNodeAttribute(uid, "x"), 0);
      const y = numberAttribute(graph.getNodeAttribute(uid, "y"), 0);
      return { uid, x, y };
    });
    const minX = Math.min(...positioned.map((node) => node.x));
    const maxX = Math.max(...positioned.map((node) => node.x));
    const minY = Math.min(...positioned.map((node) => node.y));
    const maxY = Math.max(...positioned.map((node) => node.y));
    const spanX = Math.max(maxX - minX, 1);
    const spanY = Math.max(maxY - minY, 1);
    const scale = Math.min(
      (width - padding * 2) / spanX,
      (height - padding * 2) / spanY,
    );
    const offsetX = (width - spanX * scale) / 2;
    const offsetY = (height - spanY * scale) / 2;

    const exportNodes = positioned.map(({ uid, x, y }) => {
      const label =
        stringAttribute(graph.getNodeAttribute(uid, "label"), "") ||
        uid.split(":").pop() ||
        uid;
      return {
        uid,
        x: offsetX + (x - minX) * scale,
        y: offsetY + (y - minY) * scale,
        size: Math.max(5, Math.min(28, numberAttribute(graph.getNodeAttribute(uid, "size"), 8))),
        color: stringAttribute(graph.getNodeAttribute(uid, "color"), "#64748b"),
        label,
      };
    });

    const exportEdges: ExportEdge[] = [];
    graph.forEachEdge((_edge, attributes, source, target) => {
      if (!graph.hasNode(source) || !graph.hasNode(target)) return;
      exportEdges.push({
        source,
        target,
        color: stringAttribute(attributes.color, "#94a3b8"),
        thickness: Math.max(0.5, Math.min(4, numberAttribute(attributes.thickness, 1))),
      });
    });

    const background =
      getComputedStyle(document.body).backgroundColor ||
      getComputedStyle(document.documentElement).getPropertyValue("--color-graph-bg").trim() ||
      "#ffffff";

    return {
      nodes: exportNodes,
      edges: exportEdges,
      width,
      height,
      background,
      legend: true,
    };
  };

  const exportPng = async () => {
    setError(null);
    const canvas = document.querySelector("canvas") as HTMLCanvasElement | null;
    if (!canvas) {
      setError("No graph canvas found.");
      return;
    }
    setExporting(true);
    try {
      const blob = await new Promise<Blob>((resolve, reject) => {
        canvas.toBlob((result) => {
          if (result) resolve(result);
          else reject(new Error("The graph canvas could not be captured."));
        }, "image/png");
      });
      downloadBlob(blob, "nestweaver-graph.png");
      onClose();
    } catch (err) {
      setError(err instanceof Error ? err.message : "PNG export failed.");
    } finally {
      setExporting(false);
    }
  };

  const exportServer = async (format: "svg" | "html") => {
    setError(null);
    const snapshot = buildSnapshot();
    if (!snapshot) {
      setError("No graph data is available to export.");
      return;
    }
    setExporting(true);
    try {
      const res = await fetch(`/api/v1/export/${format}`, {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify(snapshot),
      });
      if (!res.ok) {
        throw new Error(`Export failed with ${res.status}`);
      }
      const blob = await res.blob();
      downloadBlob(blob, `nestweaver-graph.${format}`);
      onClose();
    } catch (err) {
      console.error("Export failed:", err);
      setError(err instanceof Error ? err.message : `${format.toUpperCase()} export failed.`);
    } finally {
      setExporting(false);
    }
  };

  return (
    <div className="absolute right-11 top-0 z-50 min-w-40 rounded-md border border-[var(--color-border)] bg-[var(--color-surface)] py-1 shadow-xl">
      <button onClick={exportPng} disabled={exporting}
        className="w-full text-left px-3 py-1.5 text-xs hover:bg-[var(--color-surface-alt)]">
        PNG (quick capture)
      </button>
      <button onClick={() => exportServer("svg")} disabled={exporting}
        className="w-full text-left px-3 py-1.5 text-xs hover:bg-[var(--color-surface-alt)]">
        SVG (vector)
      </button>
      <button onClick={() => exportServer("html")} disabled={exporting}
        className="w-full text-left px-3 py-1.5 text-xs hover:bg-[var(--color-surface-alt)]">
        HTML (portable)
      </button>
      {exporting && <div className="px-3 py-1 text-xs text-[var(--color-text-muted)]">Exporting...</div>}
      {error && <div className="max-w-56 px-3 py-1 text-xs text-red-500">{error}</div>}
    </div>
  );
}
