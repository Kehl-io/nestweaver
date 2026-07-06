import * as Dialog from "@radix-ui/react-dialog";
import { X } from "lucide-react";
import { useStore } from "../stores";

const shortcutGroups = [
  {
    title: "Navigation",
    rows: [
      ["1", "Overview mode"],
      ["2", "Context mode"],
      ["3", "Impact mode"],
      ["4", "Repos mode"],
      ["5", "Features mode"],
      ["6", "Local mode"],
      ["/", "Focus search"],
      ["Esc", "Clear selection or close"],
    ],
  },
  {
    title: "Graph",
    rows: [
      ["M", "Toggle minimap"],
      ["C", "Toggle communities"],
      ["T", "Toggle tags"],
      ["I", "Impact analysis"],
      ["P", "Find path"],
      ["Cmd+L", "Cycle graph/list/matrix"],
      ["Cmd+Shift+G", "Toggle zen layout"],
    ],
  },
  {
    title: "Actions",
    rows: [
      ["Cmd+K", "Ask"],
      ["Cmd+Z", "Undo navigation"],
      ["Cmd+Shift+Z", "Redo navigation"],
      ["?", "Keyboard shortcuts"],
    ],
  },
];

function ShortcutKey({ children }: { children: string }) {
  return (
    <kbd className="rounded border border-[var(--color-border)] bg-[var(--color-surface-alt)] px-1.5 py-0.5 font-mono text-[10px] font-medium text-[var(--color-text)] shadow-sm">
      {children}
    </kbd>
  );
}

export function ShortcutsOverlay() {
  const open = useStore((s) => s.shortcutsOpen);
  const close = useStore((s) => s.closeShortcuts);

  return (
    <Dialog.Root open={open} onOpenChange={(nextOpen) => {
      if (!nextOpen) close();
    }}>
      <Dialog.Portal>
        <Dialog.Overlay className="fixed inset-0 z-50 bg-black/35" />
        <Dialog.Content className="fixed left-1/2 top-[12vh] z-50 flex max-h-[76vh] w-[min(42rem,calc(100vw-2rem))] -translate-x-1/2 flex-col overflow-hidden rounded-md border border-[var(--color-border)] bg-[var(--color-surface)] text-[var(--color-text)] shadow-xl focus:outline-none">
          <div className="flex shrink-0 items-center justify-between gap-3 border-b border-[var(--color-border)] px-4 py-3">
            <Dialog.Title className="text-sm font-semibold">
              Keyboard Shortcuts
            </Dialog.Title>
            <Dialog.Close
              aria-label="Close keyboard shortcuts"
              className="flex h-7 w-7 items-center justify-center rounded border border-[var(--color-border)] text-[var(--color-text-muted)] transition-colors hover:bg-[var(--color-surface-alt)] hover:text-[var(--color-text)] focus-visible:outline focus-visible:outline-2 focus-visible:outline-[var(--color-graph-selection)]"
            >
              <X size={14} aria-hidden="true" />
            </Dialog.Close>
          </div>

          <div className="grid min-h-0 gap-4 overflow-y-auto px-4 py-4 sm:grid-cols-3">
            {shortcutGroups.map((group) => (
              <section key={group.title}>
                <h3 className="mb-2 text-[11px] font-semibold uppercase text-[var(--color-text-muted)]">
                  {group.title}
                </h3>
                <div className="space-y-1.5">
                  {group.rows.map(([shortcut, label]) => (
                    <div
                      key={`${group.title}-${shortcut}`}
                      className="flex min-h-8 items-center justify-between gap-3 rounded border border-[var(--color-border)] bg-[var(--color-surface-alt)]/60 px-2 py-1.5"
                    >
                      <span className="text-xs text-[var(--color-text-muted)]">
                        {label}
                      </span>
                      <ShortcutKey>{shortcut}</ShortcutKey>
                    </div>
                  ))}
                </div>
              </section>
            ))}
          </div>
        </Dialog.Content>
      </Dialog.Portal>
    </Dialog.Root>
  );
}
