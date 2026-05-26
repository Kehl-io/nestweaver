import { useState } from "react";

interface Props {
  onClose: () => void;
}

export function ExportMenu({ onClose }: Props) {
  const [exporting, setExporting] = useState(false);

  const exportPng = () => {
    const canvas = document.querySelector("canvas") as HTMLCanvasElement | null;
    if (canvas) {
      const url = canvas.toDataURL("image/png");
      const a = document.createElement("a");
      a.href = url;
      a.download = "nestweaver-graph.png";
      a.click();
    }
    onClose();
  };

  const exportServer = async (format: "svg" | "html") => {
    setExporting(true);
    try {
      const snapshot = { nodes: [], edges: [], width: 1920, height: 1080, background: "#ffffff", legend: false };
      const res = await fetch(`/api/v1/export/${format}`, {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify(snapshot),
      });
      const blob = await res.blob();
      const url = URL.createObjectURL(blob);
      const a = document.createElement("a");
      a.href = url;
      a.download = `nestweaver-graph.${format}`;
      a.click();
      URL.revokeObjectURL(url);
    } catch (err) {
      console.error("Export failed:", err);
    } finally {
      setExporting(false);
      onClose();
    }
  };

  return (
    <div className="absolute top-full right-0 mt-1 bg-[var(--color-surface)] border border-[var(--color-border)] rounded-lg shadow-lg z-50 py-1 min-w-36">
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
    </div>
  );
}
