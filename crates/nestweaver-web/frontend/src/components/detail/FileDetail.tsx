import { useEffect, useState } from "react";
import { api } from "../../api/client";
import { sharedRepoUid, sourceErrorText } from "../../api/source";
import type { SourceResponse, SymbolCandidate } from "../../api/types";
import { useStore } from "../../stores";
import { NodeActionBar } from "../actions/NodeActionBar";
import { KindBadge } from "../shared/KindBadge";
import { CodePreview } from "./CodePreview";

interface FileDetailProps {
  path: string;
}

export function FileDetail({ path }: FileDetailProps) {
  const exploreNode = useStore((s) => s.exploreNode);
  const detailFocus = useStore((s) => s.detailFocus);
  const [symbols, setSymbols] = useState<SymbolCandidate[]>([]);
  const [source, setSource] = useState<SourceResponse | null>(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    const controller = new AbortController();
    setSymbols([]);
    setSource(null);
    setLoading(true);
    setError(null);

    // nw-683: the symbols name the repo, so fetch them first and request
    // that repo's copy of the file; a path several repos index is a 409.
    let sourceError: unknown = null;
    api
      .symbolsInFile(path)
      .catch(() => [] as SymbolCandidate[])
      .then(async (fileSymbols) => {
        const fileSource = await api
          .source(path, 1, 12, { signal: controller.signal }, sharedRepoUid(fileSymbols))
          .catch((e: unknown) => {
            sourceError = e;
            return null;
          });
        return [fileSymbols, fileSource] as const;
      })
      .then(([fileSymbols, fileSource]) => {
        if (controller.signal.aborted) return;
        setSymbols(fileSymbols);
        setSource(fileSource);
        if (fileSymbols.length === 0 && (!fileSource || !fileSource.lines?.length)) {
          setError(sourceErrorText(sourceError, path, "File evidence is unavailable."));
        }
      })
      .catch((e) => {
        if (!controller.signal.aborted) {
          setError(e instanceof Error ? e.message : "Failed to load file");
        }
      })
      .finally(() => {
        if (!controller.signal.aborted) setLoading(false);
      });

    return () => controller.abort();
  }, [path]);

  if (loading) {
    return (
      <div className="flex h-full items-center justify-center text-sm text-[var(--color-text-muted)]">
        Loading file...
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

  const line = symbols[0]?.start_line ?? source?.start_line ?? 1;
  const name = path.split("/").pop() ?? path;

  return (
    <div className="flex h-full flex-col overflow-y-auto p-4">
      <div className="mb-4">
        <div className="mb-1 flex items-center gap-2">
          <KindBadge kind="file" />
          <span className="text-sm font-semibold text-[var(--color-text)]">{name}</span>
        </div>
        <div className="mb-2 break-all text-xs text-[var(--color-text-muted)]">{path}</div>
        <div className="flex gap-4 text-xs text-[var(--color-text-muted)]">
          <span>{symbols.length} symbols</span>
        </div>
        <NodeActionBar
          node={{ uid: path, kind: "file", label: name }}
          ids={["open", "explore", "related", "copyLink"]}
          compact
          className="mt-3"
        />
      </div>

      {symbols.length > 0 && (
        <div className="mb-4">
          <h3 className="mb-1 text-xs font-semibold uppercase tracking-wide text-[var(--color-text-muted)]">
            Symbols in file
          </h3>
          <ul>
            {symbols.slice(0, 40).map((symbol) => (
              <li key={symbol.uid}>
                <button
                  type="button"
                  onClick={() => exploreNode(symbol.uid, symbol.kind)}
                  className="flex w-full items-center gap-2 rounded px-2 py-1 text-left text-xs text-[var(--color-text)] hover:bg-[var(--color-surface-alt)]"
                >
                  <KindBadge kind={symbol.kind} />
                  <span className="min-w-0 flex-1 truncate">{symbol.name}</span>
                  <span className="shrink-0 text-[10px] text-[var(--color-text-muted)]">
                    :{symbol.start_line}
                  </span>
                </button>
              </li>
            ))}
          </ul>
        </div>
      )}

      <div
        className={
          detailFocus === "source"
            ? "rounded border border-[var(--color-graph-selection)]/40 bg-[var(--color-graph-selection)]/5 p-2"
            : undefined
        }
      >
        <h3 className="mb-1 text-xs font-semibold uppercase tracking-wide text-[var(--color-text-muted)]">
          Source Evidence
        </h3>
        <CodePreview
          filePath={path}
          line={line}
          repoUid={source?.repo ?? sharedRepoUid(symbols)}
          ariaLabel={`Source evidence for ${path}`}
        />
      </div>
    </div>
  );
}
