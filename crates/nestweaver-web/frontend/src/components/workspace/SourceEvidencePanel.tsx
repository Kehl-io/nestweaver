import { useEffect, useMemo, useState } from "react";
import { FileText, Link2, SearchCode } from "lucide-react";
import { api } from "../../api/client";
import { isFileSelection, isNoteSelection, isSymbolKind } from "../../api/kinds";
import { ambiguousCandidates, sharedRepoUid, sourceErrorText } from "../../api/source";
import type { NoteDetail, SourceResponse, SymbolCandidate, SymbolDetail } from "../../api/types";
import { useStore } from "../../stores";
import { NodeActionBar } from "../actions/NodeActionBar";
import { CodePreview } from "../detail/CodePreview";
import { RepoPicker } from "../detail/RepoPicker";
import { KindBadge } from "../shared/KindBadge";

interface SourceEvidencePanelProps {
  compact?: boolean;
  className?: string;
}

function isNoteLike(uid: string | null, kind: string | null): boolean {
  return isNoteSelection(uid, kind);
}

function isSymbolLike(uid: string | null, kind: string | null): boolean {
  return Boolean(uid?.startsWith("sym:") || isSymbolKind(kind));
}

function isFileLike(uid: string | null, kind: string | null): boolean {
  return isFileSelection(uid, kind);
}

/** nw-683: the repos sharing a selected file path, and the one picked. */
interface FileRepoChoice {
  path: string;
  candidates: string[];
  repo?: string;
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
  const [fileSymbols, setFileSymbols] = useState<SymbolCandidate[]>([]);
  const [fileSource, setFileSource] = useState<SourceResponse | null>(null);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  // Kept across the refetch a pick triggers; applies only to its own path.
  const [repoChoice, setRepoChoice] = useState<FileRepoChoice | null>(null);
  const fileChoice = repoChoice && repoChoice.path === selectedNodeId ? repoChoice : null;
  const pickedRepo = fileChoice?.repo;

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

  // nw-683: a pick belongs to the path it was made for. Clear it whenever
  // the selection changes so revisiting the same path later doesn't
  // silently reapply a stale pick without a fresh ambiguity check.
  useEffect(() => {
    setRepoChoice(null);
  }, [selectedNodeId]);

  useEffect(() => {
    const controller = new AbortController();
    setSymbolDetail(null);
    setNoteDetail(null);
    setFileSymbols([]);
    setFileSource(null);
    setError(null);

    if (!selectedNodeId) {
      setLoading(false);
      return () => controller.abort();
    }

    const requestedUid = selectedNodeId;
    const isCurrent = () =>
      !controller.signal.aborted &&
      useStore.getState().selectedNodeId === requestedUid;

    if (isSymbolLike(selectedNodeId, selectedNodeKind)) {
      setLoading(true);
      api
        .symbol(selectedNodeId, { signal: controller.signal })
        .then((detail) => {
          if (isCurrent()) setSymbolDetail(detail);
        })
        .catch((e) => {
          if (isCurrent()) {
            setError(e instanceof Error ? e.message : "Symbol evidence is unavailable.");
          }
        })
        .finally(() => {
          if (isCurrent()) setLoading(false);
        });
      return () => controller.abort();
    }

    if (isNoteLike(selectedNodeId, selectedNodeKind)) {
      setLoading(true);
      api
        .brainNote(selectedNodeId, { signal: controller.signal })
        .then((detail) => {
          if (isCurrent()) setNoteDetail(detail);
        })
        .catch((e) => {
          if (isCurrent()) {
            setError(e instanceof Error ? e.message : "Note evidence is unavailable.");
          }
        })
        .finally(() => {
          if (isCurrent()) setLoading(false);
        });
      return () => controller.abort();
    }

    if (isFileLike(selectedNodeId, selectedNodeKind)) {
      setLoading(true);
      // nw-683: the symbols name the repo, so fetch them first and request
      // that repo's copy of the file; a path several repos index is a 409
      // until the user picks one, and then only that repo's symbols show.
      const path = selectedNodeId;
      let sourceError: unknown = null;
      api
        .symbolsInFile(path)
        .catch(() => [] as SymbolCandidate[])
        .then(async (allSymbols) => {
          const symbols = pickedRepo
            ? allSymbols.filter((s) => s.repo_uid === pickedRepo)
            : allSymbols;
          const source = await api
            .source(path, 1, 12, { signal: controller.signal }, pickedRepo ?? sharedRepoUid(symbols))
            .catch((e: unknown) => {
              sourceError = e;
              return null;
            });
          return [symbols, source] as const;
        })
        .then(([symbols, source]) => {
          if (controller.signal.aborted) return;
          setFileSymbols(symbols);
          setFileSource(source);
          const ambiguous = ambiguousCandidates(sourceError);
          if (ambiguous) setRepoChoice({ path, candidates: ambiguous });
          if (symbols.length === 0 && (!source || !source.lines?.length)) {
            setError(sourceErrorText(sourceError, path, "File evidence is unavailable."));
          }
        })
        .catch((e) => {
          if (!controller.signal.aborted) {
            setError(e instanceof Error ? e.message : "File evidence is unavailable.");
          }
        })
        .finally(() => {
          if (!controller.signal.aborted) setLoading(false);
        });
      return () => controller.abort();
    }

    setLoading(false);
    return () => controller.abort();
  }, [selectedNodeId, selectedNodeKind, pickedRepo]);

  const pickRepo = (uid: string, candidates: string[]) => {
    if (selectedNodeId) setRepoChoice({ path: selectedNodeId, candidates, repo: uid });
  };
  const repoPicker = fileChoice ? (
    <div className="mb-2">
      <RepoPicker
        filePath={fileChoice.path}
        candidates={fileChoice.candidates}
        selected={pickedRepo}
        onPick={(uid) => pickRepo(uid, fileChoice.candidates)}
      />
    </div>
  ) : null;

  const symbol =
    symbolDetail?.symbol.uid === selectedNodeId ? symbolDetail.symbol : undefined;
  const note =
    noteDetail?.note.uid === selectedNodeId ? noteDetail.note : undefined;
  const hasFileEvidence = Boolean(fileSource?.lines?.length || fileSymbols.length);
  const filePath = symbol?.file_path ?? graphEvidence?.filePath ?? "";
  const line =
    symbol?.start_line ??
    fileSymbols[0]?.start_line ??
    fileSource?.start_line ??
    graphEvidence?.startLine ??
    null;
  const fileLabel = selectedNodeId?.split("/").pop() ?? selectedNodeId;
  const label =
    symbol?.name ??
    note?.title ??
    (hasFileEvidence ? fileLabel : null) ??
    graphEvidence?.label ??
    "No selection";
  const kind =
    symbol?.kind ??
    (note ? "Note" : hasFileEvidence ? "file" : graphEvidence?.kind ?? selectedNodeKind);

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
        ) : loading && fileChoice ? (
          // nw-683: keep the picker mounted during the refetch a pick
          // triggers so the pressed button doesn't lose focus.
          <div>
            {repoPicker}
            <div className="rounded border border-[var(--color-border)] bg-[var(--color-surface-alt)] p-3 text-xs text-[var(--color-text-muted)]">
              Loading evidence...
            </div>
          </div>
        ) : loading ? (
          <div className="rounded border border-[var(--color-border)] bg-[var(--color-surface-alt)] p-3 text-xs text-[var(--color-text-muted)]">
            Loading evidence...
          </div>
        ) : error && fileChoice ? (
          <div>
            {pickedRepo && (
              <div className="mb-2 rounded border border-amber-500/30 bg-amber-500/10 p-3 text-xs leading-5 text-amber-200">
                {error}
              </div>
            )}
            {repoPicker}
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
            <CodePreview
              filePath={filePath}
              line={line}
              repoUid={symbol.repo_uid}
              context={compact ? 5 : 10}
            />
          </div>
        ) : hasFileEvidence && selectedNodeId ? (
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
                {selectedNodeId}
                {line != null ? `:${line}` : ""}
              </span>
            </div>
            {fileSymbols.length > 0 && (
              <p className="mb-2 text-[11px] text-[var(--color-text-muted)]">
                {fileSymbols.length} symbol{fileSymbols.length === 1 ? "" : "s"} in file
              </p>
            )}
            {repoPicker}
            {(!fileChoice || pickedRepo) && (
              <CodePreview
                filePath={selectedNodeId}
                line={line ?? 1}
                repoUid={pickedRepo ?? fileSource?.repo ?? sharedRepoUid(fileSymbols)}
                context={compact ? 5 : 10}
                onPickRepo={pickRepo}
              />
            )}
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
