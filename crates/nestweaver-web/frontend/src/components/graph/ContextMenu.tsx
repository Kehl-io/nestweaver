import { useEffect, useRef, useState } from "react";
import { useStore } from "../../stores";
import {
  type NodeAction,
  type NodeActionContext,
  type NodeActionId,
  useNodeActions,
} from "../actions/useNodeActions";

interface Props {
  x: number;
  y: number;
  nodeId: string;
  onClose: () => void;
}

type MenuItem = { action: NodeAction; hint: string; key: string | null };

const contextActionIds: NodeActionId[] = [
  "explore",
  "impact",
  "trace",
  "path",
  "ask",
  "open",
  "copyLink",
];

const shortcuts: Partial<Record<NodeActionId, { hint: string; key: string | null }>> = {
  explore: { hint: "Enter", key: null },
  impact: { hint: "I", key: "i" },
  trace: { hint: "F", key: "f" },
  path: { hint: "P", key: "p" },
  ask: { hint: "A", key: "a" },
  open: { hint: "O", key: "o" },
  copyLink: { hint: "C", key: "c" },
};

export function ContextMenu({ x, y, nodeId, onClose }: Props) {
  const graph = useStore((s) => s.graphInstance);
  const notify = useStore((s) => s.notify);

  const [focusedIndex, setFocusedIndex] = useState(0);
  const [pending, setPending] = useState<NodeActionId | null>(null);
  const containerRef = useRef<HTMLDivElement>(null);
  const itemRefs = useRef<(HTMLButtonElement | null)[]>([]);

  const node: NodeActionContext = graph?.hasNode(nodeId)
    ? {
        uid: nodeId,
        kind: (graph.getNodeAttribute(nodeId, "kind") as string | null) ?? null,
        label:
          (graph.getNodeAttribute(nodeId, "label") as string | null) ??
          nodeId.split(":").pop() ??
          nodeId,
      }
    : { uid: nodeId, kind: null, label: nodeId.split(":").pop() ?? nodeId };

  const actionItems: MenuItem[] = useNodeActions(node)
    .filter((action) => contextActionIds.includes(action.id))
    .map((action) => ({
      action,
      hint: shortcuts[action.id]?.hint ?? "",
      key: shortcuts[action.id]?.key ?? null,
    }));

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
        void runMenuAction(actionItems[focusedIndex]);
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
          void runMenuAction(actionItems[match]);
        }
        break;
      }
    }
  }

  async function runMenuAction(item: MenuItem) {
    if (item.action.disabled) {
      notify({
        kind: "warning",
        title: `${item.action.label} unavailable`,
        message: item.action.disabledReason ?? item.action.title,
      });
      return;
    }

    try {
      const result = item.action.run();
      if (result instanceof Promise) {
        setPending(item.action.id);
        await result;
      }
      onClose();
    } catch (error) {
      console.error(`${item.action.label} failed`, error);
      notify({
        kind: "error",
        title: `${item.action.label} failed`,
        message:
          error instanceof Error && error.message
            ? error.message
            : `${item.action.label} could not complete.`,
      });
    } finally {
      setPending(null);
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
      {actionItems.map((item) => {
        buttonIndex += 1;
        const idx = buttonIndex;
        const Icon = item.action.icon;
        const disabled = item.action.disabled || pending === item.action.id;

        return (
          <button
            ref={(el) => { itemRefs.current[idx] = el; }}
            id={`${menuId}-item-${idx}`}
            role="menuitem"
            key={item.action.id}
            disabled={disabled}
            title={
              item.action.disabledReason
                ? `${item.action.title}. Unavailable: ${item.action.disabledReason}`
                : item.action.title
            }
            onClick={() => void runMenuAction(item)}
            onMouseEnter={() => setFocusedIndex(idx)}
            className={
              "w-full text-left px-3 py-1.5 text-xs flex items-center justify-between gap-4 disabled:cursor-not-allowed disabled:opacity-50 " +
              (idx === focusedIndex
                ? "bg-[var(--color-surface-alt)]"
                : "hover:bg-[var(--color-surface-alt)]")
            }
          >
            <span className="flex min-w-0 items-start gap-2">
              <Icon className="mt-0.5 h-3.5 w-3.5 shrink-0 text-[var(--color-text-muted)]" />
              <span className="min-w-0">
                <span className="block truncate text-[var(--color-text)]">
                  {pending === item.action.id ? "Working" : item.action.label}
                </span>
                {item.action.disabledReason && (
                  <span className="mt-0.5 block max-w-44 text-[10px] leading-4 text-[var(--color-text-muted)]">
                    {item.action.disabledReason}
                  </span>
                )}
              </span>
            </span>
            <span className="text-[var(--color-text-muted)] font-mono shrink-0">{item.hint}</span>
          </button>
        );
      })}
    </div>
  );
}
