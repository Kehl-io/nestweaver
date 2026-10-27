import { useEffect, useRef, useState } from "react";
import { api } from "../../api/client";
import { ambiguousCandidates, sourceErrorText } from "../../api/source";
import type { SourceResponse, SymbolCandidate } from "../../api/types";
import { useStore } from "../../stores";
import { NodeActionBar } from "../actions/NodeActionBar";
import { KindBadge } from "../shared/KindBadge";
import { CodePreview } from "./CodePreview";
import { RepoPicker } from "./RepoPicker";

interface FileRepoChoice {
  path: string;
  workspaceId: string;
  candidates: string[];
  repo?: string;
}

interface FileDetailProps {
  path: string;
}

export function FileDetail({ path }: FileDetailProps) {
  const exploreNode = useStore((s) => s.exploreNode);
  const detailFocus = useStore((s) => s.detailFocus);
  const graphEpoch = useStore((s) => s.graphEpoch);
  const workspaceId = useStore((s) => s.activeWorkspaceId);
  const [loadedSymbols, setSymbols] = useState<SymbolCandidate[]>([]);
  const [loadedSource, setSource] = useState<SourceResponse | null>(null);
  const [requestLoading, setLoading] = useState(true);
  const [loadedError, setError] = useState<string | null>(null);
  // A path can survive a workspace change. Choices belong to both identities,
  // but a committed index revalidates an existing choice without unmounting it.
  const [choice, setChoice] = useState<FileRepoChoice | null>(null);
  const requestVersion = useRef(0);
  const currentChoice = choice?.path === path && choice.workspaceId === workspaceId ? choice : null;
  const candidates = currentChoice?.candidates ?? null;
  const pickedRepo = currentChoice?.repo;
  const [requestIdentity, setRequestIdentity] = useState<{
    path: string; workspaceId: string; graphEpoch: number; repo?: string;
  } | null>(null);
  const identityMatches = requestIdentity?.path === path && requestIdentity.workspaceId === workspaceId &&
    requestIdentity.graphEpoch === graphEpoch && requestIdentity.repo === pickedRepo;
  // Never paint another identity's rows/errors during the render before the
  // old effect is cleaned up. Retained valid repo buttons can stay mounted.
  const symbols = identityMatches ? loadedSymbols : [];
  const source = identityMatches ? loadedSource : null;
  const error = identityMatches ? loadedError : null;
  const loading = !identityMatches || requestLoading;

  const pickRepo = (uid: string, options: string[]) => {
    // Invalidate synchronously at the owner action, before passive cleanup.
    if (uid !== pickedRepo) requestVersion.current += 1;
    setChoice({ path, workspaceId, candidates: options, repo: uid });
  };

  useEffect(() => {
    const controller = new AbortController();
    const version = ++requestVersion.current;
    const current = () => !controller.signal.aborted && requestVersion.current === version &&
      useStore.getState().selectedNodeId === path &&
      useStore.getState().graphEpoch === graphEpoch &&
      useStore.getState().activeWorkspaceId === workspaceId;
    setRequestIdentity({ path, workspaceId, graphEpoch, repo: pickedRepo });
    setSymbols([]);
    setSource(null);
    setLoading(true);
    setError(null);

    const load = async () => {
      // Resolve ownership afresh even for a retained pick: repositories may
      // have stopped indexing this path since the last committed epoch.
      let fileSource: SourceResponse;
      try {
        fileSource = await api.source(path, 1, 12, { signal: controller.signal });
        if (!current()) return;
        if (pickedRepo && fileSource.repo !== pickedRepo) {
          setChoice(null);
          return;
        }
        setChoice(null);
      } catch (error: unknown) {
        if (!current()) return;
        const options = ambiguousCandidates(error);
        if (!options) throw error;
        const validPick = pickedRepo && options.includes(pickedRepo) ? pickedRepo : undefined;
        setChoice({ path, workspaceId, candidates: options, repo: validPick });
        if (!validPick) throw error;
        fileSource = await api.source(path, 1, 12, { signal: controller.signal }, validPick);
      }
      if (!current()) return;
      if (!fileSource.repo || (pickedRepo && fileSource.repo !== pickedRepo)) {
        throw new Error("File ownership is unavailable for the selected repository.");
      }
      const allSymbols = await api.symbolsInFile(path, { signal: controller.signal });
      if (!current()) return;
      // The list and count share the exact owner returned by /source; never
      // pool same-path symbols from other repositories (even before a pick).
      setSymbols(allSymbols.filter((symbol) => symbol.repo_uid === fileSource.repo));
      setSource(fileSource);
    };
    load()
      .catch((error: unknown) => {
        if (current()) {
          setError(sourceErrorText(error, path,
            error instanceof Error ? error.message : "File evidence is unavailable."));
        }
      })
      .finally(() => {
        if (current()) setLoading(false);
      });

    return () => controller.abort();
    // Layout/rank-only graphVersion updates must not fetch metadata or source.
  }, [path, pickedRepo, graphEpoch, workspaceId]);

  // nw-683: once a repo is picked, keep the header + picker mounted across
  // the refetch (only the symbols/source area below reflects the loading
  // state) so the pressed picker button doesn't lose focus.
  if (loading && !(candidates && pickedRepo)) {
    return (
      <div className="flex h-full items-center justify-center text-sm text-[var(--color-text-muted)]">
        Loading file...
      </div>
    );
  }

  if (error && candidates) {
    // Ambiguous path: let the user pick (or re-pick) the repo to show.
    return (
      <div className="flex h-full flex-col items-center justify-center gap-2 p-4">
        {pickedRepo && <p className="text-sm text-red-500">{error}</p>}
        <RepoPicker
          filePath={path}
          candidates={candidates}
          selected={pickedRepo}
          onPick={(uid) => pickRepo(uid, candidates)}
        />
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
        {candidates && (
          <div className="mb-2">
            <RepoPicker
              filePath={path}
              candidates={candidates}
              selected={pickedRepo}
              onPick={(uid) => pickRepo(uid, candidates)}
            />
          </div>
        )}
        {(!candidates || pickedRepo) && (
          <CodePreview
            filePath={path}
            line={line}
            repoUid={pickedRepo ?? source?.repo}
            ariaLabel={`Source evidence for ${path}`}
            onPickRepo={pickRepo}
          />
        )}
      </div>
    </div>
  );
}
