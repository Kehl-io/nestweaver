import { useStore } from "../../stores";

export function CanvasToolbar() {
  const canvasName = useStore((s) => s.canvas.name);
  const setActiveView = useStore((s) => s.setActiveView);
  const addElement = useStore((s) => s.addElement);

  const handleAddText = () => {
    addElement({
      id: `text-${Date.now()}`,
      type: "text",
      content: "New text block",
      x: 100 + Math.random() * 400,
      y: 100 + Math.random() * 300,
      width: 200,
      height: 80,
    });
  };

  const handleSave = () => {
    console.log("Canvas save triggered (placeholder)");
  };

  return (
    <div className="flex items-center gap-3 border-b border-[var(--color-border)] bg-[var(--color-bg-secondary)] px-4 py-2">
      <button
        onClick={() => setActiveView("graph")}
        className="flex items-center gap-1 text-sm text-[var(--color-text-muted)] hover:text-[var(--color-text)] transition-colors"
      >
        <span>&larr;</span>
        <span>Back to graph</span>
      </button>

      <div className="h-4 w-px bg-[var(--color-border)]" />

      <span className="text-sm font-medium text-[var(--color-text)]">
        {canvasName || "Untitled Canvas"}
      </span>

      <div className="flex-1" />

      <button
        onClick={handleAddText}
        className="rounded border border-[var(--color-border)] bg-[var(--color-bg)] px-3 py-1 text-xs font-medium text-[var(--color-text)] hover:bg-[var(--color-bg-secondary)] transition-colors"
      >
        + Text
      </button>

      <button
        onClick={handleSave}
        className="rounded bg-blue-600 px-3 py-1 text-xs font-medium text-white hover:bg-blue-700 transition-colors"
      >
        Save
      </button>
    </div>
  );
}
