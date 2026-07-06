import { useState } from "react";
import {
  type NodeActionContext,
  type NodeActionId,
  useNodeActions,
} from "./useNodeActions";
import { useStore } from "../../stores";

interface NodeActionBarProps {
  node: NodeActionContext | null;
  ids?: NodeActionId[];
  compact?: boolean;
  className?: string;
}

export function NodeActionBar({
  node,
  ids,
  compact = false,
  className = "",
}: NodeActionBarProps) {
  const actions = useNodeActions(node).filter(
    (action) => !ids || ids.includes(action.id),
  );
  const notify = useStore((s) => s.notify);
  const [pending, setPending] = useState<NodeActionId | null>(null);

  if (!node || actions.length === 0) return null;

  return (
    <div
      className={`flex flex-wrap gap-1.5 ${compact ? "text-[11px]" : "text-xs"} ${className}`}
      aria-label="Node actions"
    >
      {actions.map((action) => {
        const Icon = action.icon;
        const busy = pending === action.id;
        return (
          <button
            key={action.id}
            type="button"
            disabled={action.disabled || busy}
            title={action.title}
            onClick={async (event) => {
              event.stopPropagation();
              try {
                const result = action.run();
                if (result instanceof Promise) {
                  setPending(action.id);
                  await result;
                }
              } catch (error) {
                console.error(`${action.label} failed`, error);
                if (action.id === "compare") {
                  notify({
                    kind: "error",
                    title: "Compare failed",
                    message:
                      error instanceof Error && error.message
                        ? error.message
                        : "Context comparison request failed",
                  });
                }
              } finally {
                setPending(null);
              }
            }}
            className={`inline-flex items-center justify-center gap-1 rounded border border-[var(--color-border)] bg-[var(--color-surface)] font-medium text-[var(--color-text-muted)] transition-colors hover:bg-[var(--color-surface-alt)] hover:text-[var(--color-text)] disabled:cursor-not-allowed disabled:opacity-45 ${
              compact ? "h-7 px-1.5" : "h-8 px-2"
            }`}
          >
            <Icon className={compact ? "h-3.5 w-3.5" : "h-4 w-4"} />
            <span>{busy ? "Working" : action.label}</span>
          </button>
        );
      })}
    </div>
  );
}
