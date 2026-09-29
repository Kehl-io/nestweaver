import { useEffect, useRef, useState } from "react";
import { X } from "lucide-react";
import { useStore } from "../stores";
import { DetailPanel } from "./detail/DetailPanel";
import { ErrorBoundary } from "./ErrorBoundary";
import { SourceEvidencePanel } from "./workspace/SourceEvidencePanel";

/** Below this width the Details and Evidence panes no longer fit inline. */
export const INSPECTOR_DRAWER_QUERY = "(max-width: 899px)";

type Pane = "details" | "evidence";
const panes: { key: Pane; label: string }[] = [
  { key: "details", label: "Details" },
  { key: "evidence", label: "Evidence" },
];

/**
 * nw-593: at tablet width the inline Details/Evidence panes are hidden, so
 * they live in a non-modal drawer opened from the workspace toolbar.
 * Escape or Close returns focus to the toggle.
 */
export function InspectorDrawer() {
  const open = useStore((s) => s.inspectorOpen);
  const setOpen = useStore((s) => s.setInspectorOpen);
  const [pane, setPane] = useState<Pane>("details");
  const firstTabRef = useRef<HTMLButtonElement>(null);

  useEffect(() => {
    if (open) firstTabRef.current?.focus();
  }, [open]);

  if (!open) return null;

  const close = () => {
    setOpen(false);
    document.getElementById("inspector-toggle")?.focus();
  };

  return (
    <aside
      id="inspector-drawer"
      aria-label="Inspector"
      className="absolute inset-y-0 right-0 z-40 flex w-[min(380px,calc(100%-3rem))] flex-col border-l border-[var(--color-border)] bg-[var(--color-surface)] shadow-2xl"
      onKeyDown={(event) => {
        if (event.key === "Escape") {
          event.preventDefault();
          event.stopPropagation();
          close();
        }
      }}
    >
      <div className="flex shrink-0 items-center gap-2 border-b border-[var(--color-border)] px-2 py-1.5">
        <div
          role="tablist"
          aria-label="Inspector panes"
          className="flex flex-1 gap-1"
          onKeyDown={(event) => {
            if (event.key !== "ArrowRight" && event.key !== "ArrowLeft") return;
            event.preventDefault();
            const next = pane === "details" ? "evidence" : "details";
            setPane(next);
            document.getElementById(`inspector-tab-${next}`)?.focus();
          }}
        >
          {panes.map(({ key, label }, index) => (
            <button
              key={key}
              ref={index === 0 ? firstTabRef : undefined}
              id={`inspector-tab-${key}`}
              type="button"
              role="tab"
              aria-selected={pane === key}
              aria-controls="inspector-tabpanel"
              tabIndex={pane === key ? 0 : -1}
              onClick={() => setPane(key)}
              className={`rounded px-2.5 py-1 text-xs font-medium outline-none focus-visible:ring-2 focus-visible:ring-[var(--color-graph-selection)] ${
                pane === key
                  ? "bg-[var(--color-surface-alt)] text-[var(--color-graph-selection)]"
                  : "text-[var(--color-text-muted)] hover:text-[var(--color-text)]"
              }`}
            >
              {label}
            </button>
          ))}
        </div>
        <button
          type="button"
          onClick={close}
          aria-label="Close inspector"
          className="inline-flex h-7 w-7 items-center justify-center rounded text-[var(--color-text-muted)] outline-none hover:bg-[var(--color-surface-alt)] hover:text-[var(--color-text)] focus-visible:ring-2 focus-visible:ring-[var(--color-graph-selection)]"
        >
          <X className="h-4 w-4" aria-hidden="true" />
        </button>
      </div>
      <div
        id="inspector-tabpanel"
        role="tabpanel"
        aria-labelledby={`inspector-tab-${pane}`}
        className="min-h-0 flex-1 overflow-hidden"
      >
        <ErrorBoundary>
          {pane === "details" ? <DetailPanel /> : <SourceEvidencePanel compact />}
        </ErrorBoundary>
      </div>
    </aside>
  );
}
