import { useEffect, useRef, useState } from "react";
import { useStore } from "../../stores";
import { api } from "../../api/client";

interface Props {
  x: number;
  y: number;
  nodeId: string;
  onClose: () => void;
}

type MenuItem =
  | { label: string; hint: string; key: string | null; action: () => void }
  | "divider";

export function ContextMenu({ x, y, nodeId, onClose }: Props) {
  const exploreNode = useStore((s) => s.exploreNode);
  const setGraphMode = useStore((s) => s.setGraphMode);
  const selectNode = useStore((s) => s.selectNode);
  const setFlowTrace = useStore((s) => s.setFlowTrace);
  const startPathfinding = useStore((s) => s.startPathfinding);

  const [focusedIndex, setFocusedIndex] = useState(0);
  const containerRef = useRef<HTMLDivElement>(null);
  const itemRefs = useRef<(HTMLButtonElement | null)[]>([]);

  const menuItems: MenuItem[] = [
    // Group 1: Graph mode actions
    {
      label: "Explore",
      hint: "Enter",
      key: null,
      action: () => { exploreNode(nodeId); onClose(); },
    },
    {
      label: "Impact analysis",
      hint: "I",
      key: "i",
      action: () => { selectNode(nodeId, null); setGraphMode("impact"); onClose(); },
    },
    {
      label: "Local neighborhood",
      hint: "L",
      key: "l",
      action: () => { selectNode(nodeId, null); setGraphMode("local"); onClose(); },
    },

    "divider",

    // Group 2: Multi-node actions
    {
      label: "Find path to...",
      hint: "P",
      key: "p",
      action: () => { startPathfinding(nodeId); onClose(); },
    },
    {
      label: "Trace flow",
      hint: "F",
      key: "f",
      action: async () => {
        try {
          const result = await api.flow(nodeId, 10);
          setFlowTrace(result as any);
        } catch { /* ignore */ }
        onClose();
      },
    },

    "divider",

    // Group 3: Utilities
    {
      label: "Open source file",
      hint: "O",
      key: "o",
      action: () => {
        const graph = useStore.getState().graphInstance;
        const filePath = graph?.hasNode(nodeId)
          ? (graph.getNodeAttribute(nodeId, "file_path") as string | undefined)
          : undefined;
        if (filePath) {
          window.open(`vscode://file/${filePath}`, "_self");
        }
        onClose();
      },
    },
    {
      label: "Copy UID",
      hint: "C",
      key: "c",
      action: () => { navigator.clipboard.writeText(nodeId); onClose(); },
    },
  ];

  // Non-divider items only, for keyboard navigation
  const actionItems = menuItems.filter(
    (item): item is Exclude<MenuItem, "divider"> => item !== "divider"
  );

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
        setFocusedIndex((i) => (i + 1) % actionItems.length);
        break;
      case "ArrowUp":
        e.preventDefault();
        setFocusedIndex((i) => (i - 1 + actionItems.length) % actionItems.length);
        break;
      case "Enter":
        e.preventDefault();
        actionItems[focusedIndex].action();
        break;
      case "Escape":
        e.preventDefault();
        onClose();
        break;
      default: {
        const lower = e.key.toLowerCase();
        const match = actionItems.findIndex((a) => a.key === lower);
        if (match !== -1) {
          e.preventDefault();
          actionItems[match].action();
        }
        break;
      }
    }
  }

  const menuId = "context-menu";

  // Track button index separately from menuItems index (skip dividers)
  let buttonIndex = -1;

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
      {menuItems.map((item, i) => {
        if (item === "divider") {
          return (
            <div
              key={`divider-${i}`}
              role="separator"
              className="my-1 border-t border-[var(--color-border)]"
            />
          );
        }

        buttonIndex += 1;
        const idx = buttonIndex;

        return (
          <button
            ref={(el) => { itemRefs.current[idx] = el; }}
            id={`${menuId}-item-${idx}`}
            role="menuitem"
            key={item.label}
            onClick={item.action}
            onMouseEnter={() => setFocusedIndex(idx)}
            className={
              "w-full text-left px-3 py-1.5 text-xs flex items-center justify-between gap-4 " +
              (idx === focusedIndex
                ? "bg-[var(--color-surface-alt)]"
                : "hover:bg-[var(--color-surface-alt)]")
            }
          >
            <span>{item.label}</span>
            <span className="text-[var(--color-text-muted)] font-mono shrink-0">{item.hint}</span>
          </button>
        );
      })}
    </div>
  );
}
