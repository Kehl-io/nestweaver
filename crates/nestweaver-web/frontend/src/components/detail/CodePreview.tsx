import { useEffect, useState } from "react";
import { api } from "../../api/client";
import { ambiguousCandidates, sourceErrorText } from "../../api/source";
import type { SourceResponse } from "../../api/types";
import { RepoPicker } from "./RepoPicker";

interface CodePreviewProps {
  filePath: string;
  line: number;
  context?: number;
  ariaLabel?: string;
  /** Repo whose copy of `filePath` to show; required when several repos index it (nw-683). */
  repoUid?: string;
  /**
   * Called when the path is ambiguous and the user picks a repo (nw-683).
   * A parent that also filters other data by repo handles the pick; without
   * it the preview keeps the choice itself.
   */
  onPickRepo?: (repoUid: string, candidates: string[]) => void;
}

interface LocalPick {
  filePath: string;
  repo: string;
  candidates: string[];
}

export function CodePreview({
  filePath,
  line,
  context = 10,
  ariaLabel,
  repoUid,
  onPickRepo,
}: CodePreviewProps) {
  const [source, setSource] = useState<SourceResponse | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [candidates, setCandidates] = useState<string[] | null>(null);
  // A pick made here applies only to the path it was made for.
  const [localPick, setLocalPick] = useState<LocalPick | null>(null);
  const pick = localPick?.filePath === filePath ? localPick : null;
  const effectiveRepo = pick?.repo ?? repoUid;

  useEffect(() => {
    const controller = new AbortController();
    setSource(null);
    setError(null);
    setCandidates(null);
    api
      .source(filePath, line, context, { signal: controller.signal }, effectiveRepo)
      .then((data) => {
        if (!controller.signal.aborted) setSource(data);
      })
      .catch((e: unknown) => {
        if (!controller.signal.aborted) {
          setError(sourceErrorText(e, filePath, `Source not available: ${filePath}:${line}`));
          setCandidates(ambiguousCandidates(e));
        }
      });
    return () => controller.abort();
  }, [context, filePath, line, effectiveRepo]);

  const handlePick = (uid: string, options: string[]) => {
    if (onPickRepo) onPickRepo(uid, options);
    else setLocalPick({ filePath, repo: uid, candidates: options });
  };

  if (error && candidates) {
    return (
      <RepoPicker
        filePath={filePath}
        candidates={candidates}
        onPick={(uid) => handlePick(uid, candidates)}
      />
    );
  }

  if (error) {
    return (
      <div className="rounded border border-[var(--color-border)] bg-[var(--color-surface-alt)] px-3 py-2 text-xs text-[var(--color-text-muted)]">
        {error}
      </div>
    );
  }

  const picker = pick ? (
    <div className="mb-2">
      <RepoPicker
        filePath={filePath}
        candidates={pick.candidates}
        selected={pick.repo}
        onPick={(uid) => handlePick(uid, pick.candidates)}
      />
    </div>
  ) : null;

  if (!source || !source.lines) {
    return (
      <>
        {picker}
        <div className="rounded border border-[var(--color-border)] bg-[var(--color-surface-alt)] px-3 py-2 text-xs text-[var(--color-text-muted)]">
          Loading source...
        </div>
      </>
    );
  }

  const startLine = source.start_line ?? 1;

  return (
    <>
      {picker}
      <div
        className="overflow-x-auto rounded border border-[var(--color-border)] bg-[var(--color-surface-alt)]"
        role="region"
        aria-label={ariaLabel ?? `Source preview for ${filePath}:${line}`}
      >
        <pre className="text-xs leading-5">
          <code>
          {source.lines.map((content, i) => {
            const lineNum = startLine + i;
            const isTarget = lineNum === line;
            return (
              <span
                key={lineNum}
                aria-current={isTarget ? "true" : undefined}
                className={`block min-w-max ${isTarget ? "bg-yellow-500/20" : ""}`}
              >
                <span className="inline-block w-10 select-none pr-2 text-right text-[var(--color-text-muted)]">
                  {lineNum}
                </span>
                <span>{content}</span>
              </span>
            );
          })}
          </code>
        </pre>
      </div>
    </>
  );
}
