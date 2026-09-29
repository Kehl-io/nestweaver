import { SearchX } from "lucide-react";
import { useStore } from "../../stores";

/** The one not-found state every pane shows for a missing selection (nw-567). */
export function NodeNotFound({ uid, compact = false }: { uid: string; compact?: boolean }) {
  const selectNode = useStore((s) => s.selectNode);
  return (
    <div
      className={`rounded border border-[var(--color-border)] bg-[var(--color-surface-alt)] ${compact ? "p-3" : "m-4 p-4"} text-xs leading-5 text-[var(--color-text-muted)]`}
    >
      <div className="mb-1 flex items-center gap-2">
        <SearchX className="h-4 w-4 shrink-0 text-amber-400" aria-hidden="true" />
        <h3 className="text-sm font-semibold text-[var(--color-text)]">Node not found</h3>
      </div>
      <p>
        <span className="break-all font-mono text-[11px]">{uid}</span> is not in the index.
        It may have been renamed or removed by a re-index, or the link is mistyped.
      </p>
      <button
        type="button"
        onClick={() => selectNode(null)}
        className="mt-2 rounded border border-[var(--color-border)] px-2 py-1 text-[11px] font-medium text-[var(--color-text)] hover:bg-[var(--color-surface)] focus-visible:outline focus-visible:outline-2 focus-visible:outline-[var(--color-graph-selection)]"
      >
        Clear selection
      </button>
    </div>
  );
}
