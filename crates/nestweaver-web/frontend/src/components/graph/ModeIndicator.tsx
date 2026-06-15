import { useState, useRef, useEffect } from "react";
import { useStore } from "../../stores";
import type { GraphMode } from "../../api/types";

const modes: { key: GraphMode; label: string }[] = [
  { key: "overview", label: "Overview" },
  { key: "context", label: "Context" },
  { key: "impact", label: "Impact" },
  { key: "repos", label: "Repos" },
  { key: "features", label: "Features" },
  { key: "local", label: "Local" },
];

export function ModeIndicator() {
  const graphMode = useStore((s) => s.graphMode);
  const selectedNodeId = useStore((s) => s.selectedNodeId);
  const setGraphMode = useStore((s) => s.setGraphMode);
  const [open, setOpen] = useState(false);
  const ref = useRef<HTMLDivElement>(null);

  useEffect(() => {
    if (!open) return;
    const handler = (e: MouseEvent) => {
      if (ref.current && !ref.current.contains(e.target as Node)) setOpen(false);
    };
    document.addEventListener("mousedown", handler);
    return () => document.removeEventListener("mousedown", handler);
  }, [open]);

  const current = modes.find((m) => m.key === graphMode);
  const label = current?.label ?? graphMode;
  const nodeName = selectedNodeId?.split(":").pop() ?? "";
  const suffix = graphMode !== "overview" && nodeName ? `: ${nodeName}` : "";

  return (
    <div ref={ref} className="absolute bottom-2 right-2 z-20">
      <button
        type="button"
        onClick={() => setOpen(!open)}
        className="rounded px-2 py-1 text-[11px] text-[var(--color-text-muted)] hover:bg-[var(--color-surface-alt)] hover:text-[var(--color-text)] transition-colors"
      >
        {label}{suffix} ▾
      </button>
      {open && (
        <div className="absolute bottom-full right-0 mb-1 rounded-md border border-[var(--color-border)] bg-[var(--color-surface)] py-1 shadow-lg min-w-32">
          {modes.map((m) => (
            <button
              key={m.key}
              type="button"
              onClick={() => { setGraphMode(m.key); setOpen(false); }}
              className={`w-full text-left px-3 py-1.5 text-xs ${
                m.key === graphMode
                  ? "text-[var(--color-graph-selection)] bg-[var(--color-surface-alt)]"
                  : "text-[var(--color-text-muted)] hover:bg-[var(--color-surface-alt)] hover:text-[var(--color-text)]"
              }`}
            >
              {m.label}
            </button>
          ))}
        </div>
      )}
    </div>
  );
}
