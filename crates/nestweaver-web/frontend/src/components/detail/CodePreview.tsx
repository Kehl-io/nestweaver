import { useEffect, useState } from "react";
import { api } from "../../api/client";
import type { SourceResponse } from "../../api/types";

interface CodePreviewProps {
  filePath: string;
  line: number;
  context?: number;
  ariaLabel?: string;
}

export function CodePreview({
  filePath,
  line,
  context = 10,
  ariaLabel,
}: CodePreviewProps) {
  const [source, setSource] = useState<SourceResponse | null>(null);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    const controller = new AbortController();
    setSource(null);
    setError(null);
    api
      .source(filePath, line, context)
      .then((data) => {
        if (!controller.signal.aborted) setSource(data);
      })
      .catch(() => {
        if (!controller.signal.aborted) setError(`Source not available: ${filePath}:${line}`);
      });
    return () => controller.abort();
  }, [context, filePath, line]);

  if (error) {
    return (
      <div className="rounded border border-[var(--color-border)] bg-[var(--color-surface-alt)] px-3 py-2 text-xs text-[var(--color-text-muted)]">
        {error}
      </div>
    );
  }

  if (!source || !source.lines) {
    return (
      <div className="rounded border border-[var(--color-border)] bg-[var(--color-surface-alt)] px-3 py-2 text-xs text-[var(--color-text-muted)]">
        Loading source...
      </div>
    );
  }

  const startLine = source.start_line ?? 1;

  return (
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
  );
}
