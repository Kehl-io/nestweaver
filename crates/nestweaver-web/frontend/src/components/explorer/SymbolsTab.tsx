import { useEffect, useMemo, useState } from "react";
import * as Select from "@radix-ui/react-select";
import { ChevronDown } from "lucide-react";
import { api } from "../../api/client";
import type { SymbolCandidate } from "../../api/types";
import { useStore } from "../../stores";
import { KindBadge } from "../shared/KindBadge";

const KIND_OPTIONS = ["All", "Function", "Class", "Method", "Interface"] as const;
const MAX_VISIBLE = 100;

export function SymbolsTab() {
  const selectedNodeId = useStore((s) => s.selectedNodeId);
  const exploreNode = useStore((s) => s.exploreNode);
  const activeWorkspaceId = useStore((s) => s.activeWorkspaceId);

  const [symbols, setSymbols] = useState<SymbolCandidate[]>([]);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [filter, setFilter] = useState("");
  const [kindFilter, setKindFilter] = useState<string>("All");

  useEffect(() => {
    setLoading(true);
    setError(null);
    api
      .symbolsTop(200, activeWorkspaceId)
      .then(setSymbols)
      .catch((e) => setError(e.message ?? "Failed to load symbols"))
      .finally(() => setLoading(false));
  }, [activeWorkspaceId]);

  const filtered = useMemo(() => {
    const lc = filter.toLowerCase();
    return symbols.filter((s) => {
      if (kindFilter !== "All" && s.kind !== kindFilter) return false;
      if (lc && !s.name.toLowerCase().includes(lc)) return false;
      return true;
    });
  }, [symbols, filter, kindFilter]);

  const visible = filtered.slice(0, MAX_VISIBLE);
  const truncated = filtered.length > MAX_VISIBLE;

  if (loading) {
    return (
      <div className="flex h-full items-center justify-center text-sm text-[var(--color-text-muted)]">
        Loading symbols...
      </div>
    );
  }

  if (error) {
    return (
      <div className="flex h-full items-center justify-center p-4 text-sm text-red-500">
        {error}
      </div>
    );
  }

  return (
    <div className="flex h-full flex-col overflow-hidden">
      <div className="flex gap-1 border-b border-[var(--color-border)] p-2">
        <input
          type="text"
          placeholder="Filter symbols..."
          value={filter}
          onChange={(e) => setFilter(e.target.value)}
          className="min-w-0 flex-1 rounded border border-[var(--color-border)] bg-[var(--color-surface-alt)] px-2 py-1 text-xs text-[var(--color-text)] placeholder:text-[var(--color-text-muted)] focus:outline-none focus:ring-1 focus:ring-[var(--color-graph-selection)]"
        />
        <Select.Root value={kindFilter} onValueChange={setKindFilter}>
          <Select.Trigger
            aria-label="Kind"
            className="inline-flex h-7 w-[6.75rem] shrink-0 items-center justify-between gap-1 rounded border border-[var(--color-border)] bg-[var(--color-surface-alt)] px-2 text-xs text-[var(--color-text)] outline-none focus-visible:ring-1 focus-visible:ring-[var(--color-graph-selection)]"
          >
            <Select.Value />
            <Select.Icon asChild>
              <ChevronDown className="h-3.5 w-3.5 shrink-0 text-[var(--color-text-muted)]" />
            </Select.Icon>
          </Select.Trigger>
          <Select.Portal>
            <Select.Content
              position="popper"
              sideOffset={4}
              className="z-[100] min-w-[var(--radix-select-trigger-width)] overflow-hidden rounded-md border border-[var(--color-border)] bg-[var(--color-surface)] py-1 text-xs text-[var(--color-text)] shadow-xl"
            >
              <Select.Viewport>
                {KIND_OPTIONS.map((k) => (
                  <Select.Item
                    key={k}
                    value={k}
                    className="relative cursor-default select-none px-3 py-1.5 outline-none data-[highlighted]:bg-[var(--color-surface-alt)] data-[state=checked]:text-[var(--color-graph-selection)]"
                  >
                    <Select.ItemText>{k}</Select.ItemText>
                  </Select.Item>
                ))}
              </Select.Viewport>
            </Select.Content>
          </Select.Portal>
        </Select.Root>
      </div>

      <div className="flex-1 overflow-y-auto">
        {visible.length === 0 ? (
          <div className="p-4 text-center text-xs text-[var(--color-text-muted)]">
            No symbols match the current filter.
          </div>
        ) : (
          <ul>
            {visible.map((sym) => {
              const selected = selectedNodeId === sym.uid;
              const fileName = sym.file_path.split("/").pop() ?? sym.file_path;
              return (
                <li key={sym.uid}>
                  <button
                    type="button"
                    onClick={() => {
                      exploreNode(sym.uid, sym.kind);
                    }}
                    className={`flex w-full items-center gap-2 px-2 py-1.5 text-left text-xs transition-colors ${
                      selected
                        ? "bg-blue-600/15 text-blue-600"
                        : "text-[var(--color-text)] hover:bg-[var(--color-surface-alt)]"
                    }`}
                  >
                    <KindBadge kind={sym.kind} />
                    <span className="min-w-0 flex-1 truncate font-medium">
                      {sym.name}
                    </span>
                    <span className="shrink-0 truncate text-[10px] text-[var(--color-text-muted)]">
                      {fileName}
                    </span>
                  </button>
                </li>
              );
            })}
          </ul>
        )}
      </div>

      <div className="border-t border-[var(--color-border)] px-2 py-1 text-[10px] text-[var(--color-text-muted)]">
        {truncated
          ? `Showing ${MAX_VISIBLE} of ${filtered.length}`
          : `Showing ${filtered.length} of ${symbols.length}`}
      </div>
    </div>
  );
}
