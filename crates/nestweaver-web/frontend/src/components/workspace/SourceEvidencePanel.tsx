import { useEffect, useMemo, useState } from "react";
import { FileText, Link2, SearchCode } from "lucide-react";
import { api } from "../../api/client";
import { isSymbolKind } from "../../api/kinds";
import type { NoteDetail, SymbolDetail } from "../../api/types";
import { useStore } from "../../stores";
import { NodeActionBar } from "../actions/NodeActionBar";
import { CodePreview } from "../detail/CodePreview";
import { KindBadge } from "../shared/KindBadge";

interface SourceEvidencePanelProps {
  compact?: boolean;
  className?: string;
}

function isNoteLike(uid: string | null, kind: string | null): boolean {
  return Boolean(uid?.startsWith("note:") || kind === "note" || kind === "Note");
}

function isSymbolLike(uid: string | null, kind: string | null): boolean {
  return Boolean(uid?.startsWith("sym:") || isSymbolKind(kind));
}

function noteSnippet(body: string): string {
  const normalized = body.trim().replace(/\n{3,}/g, "\n\n");
  if (normalized.length <= 1600) return normalized;
  return `${normalized.slice(0, 1600).trimEnd()}\n\n...`;
}

export function SourceEvidencePanel({
  compact = false,
  className = "",
}: SourceEvidencePanelProps) {
  const selectedNodeId = useStore((s) => s.selectedNodeId);
  const selectedNodeKind = useStore((s) => s.selectedNodeKind);
  const graphInstance = useStore((s) => s.graphInstance);
  const detailFocus = useStore((s) => s.detailFocus);
  const [symbolDetail, setSymbolDetail] = useState<SymbolDetail | null>(null);
  const [noteDetail, setNoteDetail] = useState<NoteDetail | null>(null);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const graphEvidence = useMemo(() => {
    if (!selectedNodeId || !graphInstance?.hasNode(selectedNodeId)) return null;
    return {
      label:
        (graphInstance.getNodeAttribute(selectedNodeId, "label") as string | undefined) ??
        selectedNodeId,
      kind:
        (graphInstance.getNodeAttribute(selectedNodeId, "kind") as string | undefined) ??
        selectedNodeKind ??
        "node",
      location:
        (graphInstance.getNodeAttribute(selectedNodeId, "location") as string | undefined) ??
        (graphInstance.getNodeAttribute(selectedNodeId, "filePath") as string | undefined) ??
        "",
      filePath:
        (graphInstance.getNodeAttribute(selectedNodeId, "filePath") as string | undefined) ??
        (graphInstance.getNodeAttribute(selectedNodeId, "file_path") as string | undefined) ??
        "",
      startLine:
        (graphInstance.getNodeAttribute(selectedNodeId, "startLine") as number | undefined) ??
        (graphInstance.getNodeAttribute(selectedNodeId, "start_line") as number | undefined) ??
        null,
      reason:
        (graphInstance.getNodeAttribute(selectedNodeId, "reason") as string | undefined) ??
        "",
    };
  }, [graphInstance, selectedNodeId, selectedNodeKind]);

  useEffect(() => {
    const controller = new AbortController();
    setSymbolDetail(null);
    setNoteDetail(null);
    setError(null);

    if (!selectedNodeId) {
      setLoading(false);
      return () => controller.abort();
    }

    if (isSymbolLike(selectedNodeId, selectedNodeKind)) {
      setLoading(true);
      api
        .symbol(selectedNodeId)
        .then((detail) => {
          if (!controller.signal.aborted) setSymbolDetail(detail);
        })
        .catch((e) => {
          if (!controller.signal.aborted) {
            setError(e instanceof Error ? e.message : "Symbol evidence is unavailable.");
          }
        })
        .finally(() => {
          if (!controller.signal.aborted) setLoading(false);
        });
      return () => controller.abort();
    }

    if (isNoteLike(selectedNodeId, selectedNodeKind)) {
      setLoading(true);
      api
        .brainNote(selectedNodeId)
        .then((detail) => {
          if (!controller.signal.aborted) setNoteDetail(detail);
        })
        .catch((e) => {
          if (!controller.signal.aborted) {
            setError(e instanceof Error ? e.message : "Note evidence is unavailable.");
          }
        })
        .finally(() => {
          if (!controller.signal.aborted) setLoading(false);
        });
      return () => controller.abort();
    }

    setLoading(false);
    return () => controller.abort();
  }, [selectedNodeId, selectedNodeKind]);

  const symbol = symbolDetail?.symbol;
  const note = noteDetail?.note;
  const filePath = symbol?.file_path ?? graphEvidence?.filePath ?? "";
  const line = symbol?.start_line ?? graphEvidence?.startLine ?? null;
  const label = symbol?.name ?? note?.title ?? graphEvidence?.label ?? "No selection";
  const kind = symbol?.kind ?? (note ? "Note" : graphEvidence?.kind ?? selectedNodeKind);

  return (
    <aside
      aria-label="Source and note evidence"
      className={`flex h-full min-h-0 flex-col border-l border-[var(--color-border)] bg-[var(--color-surface)] ${className}`}
    >
      <div className="shrink-0 border-b border-[var(--color-border)] p-3">
        <div className="flex min-w-0 items-start justify-between gap-2">
          <div className="min-w-0">
            <p className="text-[10px] font-semibold uppercase tracking-wide text-[var(--color-text-muted)]">
              Evidence
            </p>
            <h2 className="mt-1 truncate text-sm font-semibold text-[var(--color-text)]">
              {label}
            </h2>
          </div>
          {kind && <KindBadge kind={kind} />}
        </div>
        {selectedNodeId && (
          <NodeActionBar
            node={{ uid: selectedNodeId, kind, label }}
            ids={["open", "related", "trace", "copyLink"]}
            compact
            className="mt-3"
          />
        )}
      </div>

      <div className="min-h-0 flex-1 overflow-y-auto p-3">
        {!selectedNodeId ? (
          <div className="rounded border border-[var(--color-border)] bg-[var(--color-surface-alt)] p-3 text-xs leading-5 text-[var(--color-text-muted)]">
            Select a node to inspect source spans, note excerpts, or an explicit
            no-evidence state.
          </div>
        ) : loading ? (
          <div className="rounded border border-[var(--color-border)] bg-[var(--color-surface-alt)] p-3 text-xs text-[var(--color-text-muted)]">
            Loading evidence...
          </div>
        ) : error ? (
          <div className="rounded border border-amber-500/30 bg-amber-500/10 p-3 text-xs leading-5 text-amber-200">
            {error}
          </div>
        ) : symbol && filePath && line != null ? (
          <div
            className={
              detailFocus === "source"
                ? "rounded border border-[var(--color-graph-selection)]/50 bg-[var(--color-graph-selection)]/5 p-2"
                : ""
            }
          >
            <div className="mb-2 flex items-center gap-2 text-[11px] text-[var(--color-text-muted)]">
              <SearchCode className="h-3.5 w-3.5" />
              <span className="min-w-0 truncate">
                {filePath}:{line}
              </span>
            </div>
            {symbol.signature && (
              <pre className="mb-2 overflow-x-auto rounded border border-[var(--color-border)] bg-[var(--color-surface-alt)] p-2 text-[11px] text-[var(--color-text)]">
                <code>{symbol.signature}</code>
              </pre>
            )}
            <CodePreview filePath={filePath} line={line} context={compact ? 5 : 10} />
          </div>
        ) : note && noteDetail ? (
          <div
            className={
              detailFocus === "source"
                ? "rounded border border-[var(--color-graph-selection)]/50 bg-[var(--color-graph-selection)]/5 p-2"
                : ""
            }
          >
            <div className="mb-2 flex items-center gap-2 text-[11px] text-[var(--color-text-muted)]">
              <FileText className="h-3.5 w-3.5" />
              <span className="min-w-0 truncate">{note.file_path}</span>
            </div>
            <pre className="max-h-[28rem] overflow-auto whitespace-pre-wrap rounded border border-[var(--color-border)] bg-[var(--color-surface-alt)] p-3 text-xs leading-5 text-[var(--color-text)]">
              {noteSnippet(noteDetail.body)}
            </pre>
            {noteDetail.headings.length > 0 && (
              <div className="mt-3 rounded border border-[var(--color-border)] bg-[var(--color-surface-alt)] p-2">
                <p className="mb-1 text-[10px] font-semibold uppercase tracking-wide text-[var(--color-text-muted)]">
                  Outline Evidence
                </p>
                <ul className="space-y-1 text-xs text-[var(--color-text-muted)]">
                  {noteDetail.headings.slice(0, 8).map((heading) => (
                    <li key={heading.uid} className="flex items-center gap-1.5">
                      <Link2 className="h-3 w-3 shrink-0" />
                      <span className="truncate">{heading.text}</span>
                    </li>
                  ))}
                </ul>
              </div>
            )}
          </div>
        ) : graphEvidence ? (
          <div className="space-y-3">
            <div className="rounded border border-[var(--color-border)] bg-[var(--color-surface-alt)] p-3 text-xs leading-5 text-[var(--color-text-muted)]">
              {graphEvidence.reason ||
                "This graph node does not expose a source span or note body through the current P1 API."}
            </div>
            {graphEvidence.location && (
              <p className="break-all text-[11px] text-[var(--color-text-muted)]">
                Location: {graphEvidence.location}
              </p>
            )}
          </div>
        ) : (
          <div className="rounded border border-[var(--color-border)] bg-[var(--color-surface-alt)] p-3 text-xs leading-5 text-[var(--color-text-muted)]">
            Evidence is unavailable for this selection.
          </div>
        )}
      </div>
    </aside>
  );
}
