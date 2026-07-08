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
  const layoutClass = className.includes("grid") ? "" : "flex flex-wrap";

  if (!node || actions.length === 0) return null;

  return (
    <div
      className={`${layoutClass} gap-1.5 ${compact ? "text-[11px]" : "text-xs"} ${className}`}
      aria-label="Node actions"
    >
      {actions.map((action) => {
        const Icon = action.icon;
        const busy = pending === action.id;
        const unavailable = Boolean(action.disabled);
        const disabledTitle = action.disabledReason
          ? `${action.title}. Unavailable: ${action.disabledReason}`
          : action.title;
        return (
          <button
            key={action.id}
            type="button"
            disabled={busy}
            aria-disabled={unavailable}
            title={disabledTitle}
            aria-label={
              action.disabledReason
                ? `${action.label}, unavailable: ${action.disabledReason}`
                : action.label
            }
            onClick={async (event) => {
              event.stopPropagation();
              if (unavailable) {
                notify({
                  kind: "warning",
                  title: `${action.label} unavailable`,
                  message: action.disabledReason ?? action.title,
                });
                return;
              }
              try {
                const result = action.run();
                if (result instanceof Promise) {
                  setPending(action.id);
                  await result;
                }
              } catch (error) {
                console.error(`${action.label} failed`, error);
                notify({
                  kind: "error",
                  title: `${action.label} failed`,
                  message:
                    error instanceof Error && error.message
                      ? error.message
                      : `${action.label} could not complete.`,
                });
              } finally {
                setPending(null);
              }
            }}
            className={`inline-flex items-center justify-center gap-1 rounded border border-[var(--color-border)] bg-[var(--color-surface)] font-medium text-[var(--color-text-muted)] transition-colors hover:bg-[var(--color-surface-alt)] hover:text-[var(--color-text)] disabled:cursor-wait disabled:opacity-45 ${
              compact ? "h-7 px-1.5" : "h-8 px-2"
            } ${unavailable ? "cursor-not-allowed opacity-55" : ""}`}
          >
            <Icon className={compact ? "h-3.5 w-3.5" : "h-4 w-4"} />
            <span>{busy ? "Working" : action.label}</span>
            {action.disabledReason && (
              <span className="sr-only">Unavailable: {action.disabledReason}</span>
            )}
          </button>
        );
      })}
    </div>
  );
}
