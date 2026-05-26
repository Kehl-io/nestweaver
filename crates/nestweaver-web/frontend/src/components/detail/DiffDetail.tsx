import { useStore } from "../../stores";

export function DiffDetail() {
  const diffState = useStore((s) => s.diffState);
  const clearDiff = useStore((s) => s.clearDiff);

  if (!diffState.snapshotA || !diffState.snapshotB) return null;

  const uidsA = new Set(
    [...diffState.snapshotA.seeds, ...diffState.snapshotA.connected].map(
      (n) => n.uid,
    ),
  );
  const uidsB = new Set(
    [...diffState.snapshotB.seeds, ...diffState.snapshotB.connected].map(
      (n) => n.uid,
    ),
  );
  const shared = [...uidsA].filter((uid) => uidsB.has(uid));
  const onlyA = [...uidsA].filter((uid) => !uidsB.has(uid));
  const onlyB = [...uidsB].filter((uid) => !uidsA.has(uid));

  return (
    <div className="p-3 text-sm border-b border-[var(--color-border)]">
      <div className="flex items-center justify-between mb-2">
        <h3 className="font-semibold text-xs uppercase text-[var(--color-text-muted)]">
          Diff
        </h3>
        <button
          onClick={clearDiff}
          className="text-xs text-blue-500 hover:underline"
        >
          Clear
        </button>
      </div>
      <div className="text-xs space-y-1 mb-3">
        <p>A: {diffState.seedsA.join(", ")}</p>
        <p>B: {diffState.seedsB.join(", ")}</p>
      </div>
      <div className="flex gap-2 text-xs mb-3">
        <span className="px-2 py-0.5 bg-[var(--color-surface-alt)] rounded">
          {shared.length} shared
        </span>
        <span className="px-2 py-0.5 bg-blue-500/15 text-blue-400 rounded">
          {onlyA.length} only A
        </span>
        <span className="px-2 py-0.5 bg-green-500/15 text-green-400 rounded">
          {onlyB.length} only B
        </span>
      </div>
      {onlyA.length > 0 && (
        <div className="mb-2">
          <p className="text-xs font-medium text-blue-600 mb-1">Only in A</p>
          {onlyA.slice(0, 10).map((uid) => (
            <p
              key={uid}
              className="text-[10px] text-[var(--color-text-muted)] truncate"
            >
              {uid}
            </p>
          ))}
        </div>
      )}
      {onlyB.length > 0 && (
        <div>
          <p className="text-xs font-medium text-green-600 mb-1">Only in B</p>
          {onlyB.slice(0, 10).map((uid) => (
            <p
              key={uid}
              className="text-[10px] text-[var(--color-text-muted)] truncate"
            >
              {uid}
            </p>
          ))}
        </div>
      )}
    </div>
  );
}
