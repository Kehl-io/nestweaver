import { useStore } from "../../stores";

export function GapDetail() {
  const gapItems = useStore((s) => s.gapItems);
  const toggleGapPanel = useStore((s) => s.toggleGapPanel);
  const selectNode = useStore((s) => s.selectNode);

  return (
    <div className="p-3 text-sm border-b border-[var(--color-border)]">
      <div className="flex items-center justify-between mb-2">
        <h3 className="font-semibold text-xs uppercase text-[var(--color-text-muted)]">
          Structural Gaps ({gapItems.length})
        </h3>
        <button
          onClick={toggleGapPanel}
          className="text-xs text-blue-500 hover:underline"
        >
          Close
        </button>
      </div>
      {gapItems.length === 0 ? (
        <p className="text-xs text-[var(--color-text-muted)]">
          No gaps detected
        </p>
      ) : (
        <div className="space-y-2">
          {gapItems.map((item, i) => (
            <button
              key={i}
              onClick={() => {
                if (item.nodeUids.length > 0)
                  selectNode(item.nodeUids[0], null);
              }}
              className="w-full text-left p-2 rounded border border-[var(--color-border)] hover:bg-[var(--color-surface-alt)]"
            >
              <div className="flex items-center gap-1.5 mb-0.5">
                <span
                  className={`text-[10px] px-1.5 py-0.5 rounded font-medium ${
                    item.type === "undocumented"
                      ? "bg-amber-500/15 text-amber-400"
                      : item.type === "untested"
                        ? "bg-red-500/15 text-red-400"
                        : "bg-blue-500/15 text-blue-400"
                  }`}
                >
                  {item.type}
                </span>
                <span className="text-xs font-medium truncate">
                  {item.label}
                </span>
              </div>
              <p className="text-[10px] text-[var(--color-text-muted)]">
                {item.detail}
              </p>
            </button>
          ))}
        </div>
      )}
    </div>
  );
}
