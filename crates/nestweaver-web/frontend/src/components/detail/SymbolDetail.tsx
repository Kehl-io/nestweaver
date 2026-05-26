import { useEffect, useState } from "react";
import { api } from "../../api/client";
import type { SymbolDetail as SymbolDetailType } from "../../api/types";
import { useStore } from "../../stores";
import { Collapsible } from "../shared/Collapsible";
import { KindBadge } from "../shared/KindBadge";
import { CodePreview } from "./CodePreview";

interface SymbolDetailProps {
  uid: string;
}

export function SymbolDetail({ uid }: SymbolDetailProps) {
  const selectNode = useStore((s) => s.selectNode);
  const setSeeds = useStore((s) => s.setSeeds);

  const [detail, setDetail] = useState<SymbolDetailType | null>(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    const controller = new AbortController();
    setDetail(null);
    setLoading(true);
    setError(null);
    api
      .symbol(uid)
      .then((data) => {
        if (!controller.signal.aborted) setDetail(data);
      })
      .catch((e) => {
        if (!controller.signal.aborted) setError(e.message ?? "Failed to load symbol");
      })
      .finally(() => {
        if (!controller.signal.aborted) setLoading(false);
      });
    return () => controller.abort();
  }, [uid]);

  if (loading) {
    return (
      <div className="flex h-full items-center justify-center text-sm text-[var(--color-text-muted)]">
        Loading symbol...
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

  if (!detail) {
    return (
      <div className="flex h-full items-center justify-center text-sm text-[var(--color-text-muted)]">
        Symbol not found.
      </div>
    );
  }

  const { symbol, callers, callees } = detail;
  const refCount = callers.length + callees.length;

  const handleRefClick = (refUid: string, kind: string) => {
    selectNode(refUid, kind);
    setSeeds([refUid]);
  };

  return (
    <div className="flex h-full flex-col overflow-y-auto p-4">
      {/* Top: Identity */}
      <div className="mb-4">
        <div className="mb-1 flex items-center gap-2">
          <KindBadge kind={symbol.kind} />
          <span className="text-sm font-semibold text-[var(--color-text)]">
            {symbol.name}
          </span>
        </div>
        <div className="mb-2 text-xs text-[var(--color-text-muted)]">
          {symbol.file_path}:{symbol.start_line}
        </div>
        {symbol.signature && (
          <pre className="mb-2 overflow-x-auto rounded border border-[var(--color-border)] bg-[var(--color-surface-alt)] p-2 text-xs">
            {symbol.signature}
          </pre>
        )}
        <div className="flex gap-4 text-xs text-[var(--color-text-muted)]">
          <span>PageRank: {symbol.pagerank_score.toFixed(4)}</span>
          <span>
            Refs: {refCount}
          </span>
        </div>
      </div>

      {/* Middle: Callers & Callees */}
      <div className="mb-4 space-y-1">
        <Collapsible title="Callers" count={callers.length} defaultOpen={callers.length > 0}>
          {callers.length === 0 ? (
            <div className="px-4 py-1 text-[10px] text-[var(--color-text-muted)]">
              No callers.
            </div>
          ) : (
            <ul>
              {callers.map((c) => (
                <li key={c.uid}>
                  <button
                    type="button"
                    onClick={() => handleRefClick(c.uid, c.kind)}
                    className="flex w-full items-center gap-2 px-4 py-1 text-left text-xs text-[var(--color-text)] hover:bg-[var(--color-surface-alt)]"
                  >
                    <KindBadge kind={c.kind} />
                    <span className="min-w-0 flex-1 truncate">{c.name}</span>
                    <span className="shrink-0 text-[10px] text-[var(--color-text-muted)]">
                      {c.file_path.split("/").pop()}
                    </span>
                  </button>
                </li>
              ))}
            </ul>
          )}
        </Collapsible>

        <Collapsible title="Callees" count={callees.length} defaultOpen={callees.length > 0}>
          {callees.length === 0 ? (
            <div className="px-4 py-1 text-[10px] text-[var(--color-text-muted)]">
              No callees.
            </div>
          ) : (
            <ul>
              {callees.map((c) => (
                <li key={c.uid}>
                  <button
                    type="button"
                    onClick={() => handleRefClick(c.uid, c.kind)}
                    className="flex w-full items-center gap-2 px-4 py-1 text-left text-xs text-[var(--color-text)] hover:bg-[var(--color-surface-alt)]"
                  >
                    <KindBadge kind={c.kind} />
                    <span className="min-w-0 flex-1 truncate">{c.name}</span>
                    <span className="shrink-0 text-[10px] text-[var(--color-text-muted)]">
                      {c.file_path.split("/").pop()}
                    </span>
                  </button>
                </li>
              ))}
            </ul>
          )}
        </Collapsible>
      </div>

      {/* Bottom: Code Preview */}
      <div>
        <h3 className="mb-1 text-xs font-semibold uppercase tracking-wide text-[var(--color-text-muted)]">
          Source
        </h3>
        <CodePreview filePath={symbol.file_path} line={symbol.start_line} />
      </div>
    </div>
  );
}
