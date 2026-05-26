import { useEffect, useRef, useState } from "react";
import { useStore } from "../../stores";
import { api } from "../../api/client";

interface Props {
  x: number;
  y: number;
  nodeId: string;
  onClose: () => void;
}

export function ContextMenu({ x, y, nodeId, onClose }: Props) {
  const setSeeds = useStore((s) => s.setSeeds);
  const setGraphMode = useStore((s) => s.setGraphMode);
  const selectNode = useStore((s) => s.selectNode);
  const setFlowTrace = useStore((s) => s.setFlowTrace);
  const startPathfinding = useStore((s) => s.startPathfinding);

  const [focusedIndex, setFocusedIndex] = useState(0);
  const containerRef = useRef<HTMLDivElement>(null);
  const itemRefs = useRef<(HTMLButtonElement | null)[]>([]);

  const actions = [
    { label: "Re-seed context from here", hint: "Enter", key: null,
      action: () => { setSeeds([nodeId]); onClose(); } },
    { label: "Impact analysis...", hint: "I", key: "i",
      action: () => { selectNode(nodeId, null); setGraphMode("impact"); onClose(); } },
    { label: "Trace flow...", hint: "F", key: "f",
      action: async () => {
        try {
          const result = await api.flow(nodeId, 10);
          setFlowTrace(result as any);
        } catch { /* ignore */ }
        onClose();
      },
    },
    { label: "Find path to...", hint: "P", key: "p",
      action: () => { startPathfinding(nodeId); onClose(); },
    },
    { label: "Copy UID", hint: "C", key: "c",
      action: () => { navigator.clipboard.writeText(nodeId); onClose(); } },
  ];

  // Auto-focus the container when the menu opens
  useEffect(() => {
    containerRef.current?.focus();
  }, []);

  // Keep the focused item's button scrolled into view
  useEffect(() => {
    itemRefs.current[focusedIndex]?.scrollIntoView({ block: "nearest" });
  }, [focusedIndex]);

  function handleKeyDown(e: React.KeyboardEvent<HTMLDivElement>) {
    switch (e.key) {
      case "ArrowDown":
        e.preventDefault();
        setFocusedIndex((i) => (i + 1) % actions.length);
        break;
      case "ArrowUp":
        e.preventDefault();
        setFocusedIndex((i) => (i - 1 + actions.length) % actions.length);
        break;
      case "Enter":
        e.preventDefault();
        actions[focusedIndex].action();
        break;
      case "Escape":
        e.preventDefault();
        onClose();
        break;
      default: {
        const lower = e.key.toLowerCase();
        const match = actions.findIndex((a) => a.key === lower);
        if (match !== -1) {
          e.preventDefault();
          actions[match].action();
        }
        break;
      }
    }
  }

  const menuId = "context-menu";

  return (
    <div
      ref={containerRef}
      id={menuId}
      role="menu"
      tabIndex={-1}
      aria-activedescendant={`${menuId}-item-${focusedIndex}`}
      onKeyDown={handleKeyDown}
      className="fixed z-50 bg-[var(--color-surface)] border border-[var(--color-border)] rounded-lg shadow-lg py-1 min-w-44 outline-none"
      style={{ left: x, top: y }}
    >
      {actions.map((a, i) => (
        <button
          ref={(el) => { itemRefs.current[i] = el; }}
          id={`${menuId}-item-${i}`}
          role="menuitem"
          key={a.label}
          onClick={a.action}
          onMouseEnter={() => setFocusedIndex(i)}
          className={
            "w-full text-left px-3 py-1.5 text-xs flex items-center justify-between gap-4 " +
            (i === focusedIndex
              ? "bg-[var(--color-surface-alt)]"
              : "hover:bg-[var(--color-surface-alt)]")
          }
        >
          <span>{a.label}</span>
          <span className="text-[var(--color-text-muted)] font-mono shrink-0">{a.hint}</span>
        </button>
      ))}
    </div>
  );
}
