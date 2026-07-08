import { useEffect, useMemo, useRef } from "react";
import { useStore } from "../stores";
import { loadGapItems } from "../api/client";
import {
  executeSearchPhrase,
  parseSearchPhrase,
  PhrasePreview,
  resolveSearchPhrase,
  type PhraseCandidate,
  type PhraseCandidateOverrides,
} from "../searchPhrases";
import { GlassPanel } from "./panels/GlassPanel";
import { KindBadge } from "./shared/KindBadge";

interface SearchDropdownProps {
  onSelect: (uid: string, kind: string) => void;
  activeDescendant?: string;
}

export function SearchDropdown({ onSelect, activeDescendant }: SearchDropdownProps) {
  const searchQuery = useStore((s) => s.searchQuery);
  const searchLoading = useStore((s) => s.searchLoading);
  const searchResults = useStore((s) => s.searchResults);
  const brainSearchResults = useStore((s) => s.brainSearchResults);
  const phraseIntent = useStore((s) => s.phraseIntent);
  const phraseResolution = useStore((s) => s.phraseResolution);
  const phraseResolving = useStore((s) => s.phraseResolving);
  const phraseError = useStore((s) => s.phraseError);
  const activeWorkspaceId = useStore((s) => s.activeWorkspaceId);
  const workspaces = useStore((s) => s.workspaces);
  const selectNode = useStore((s) => s.selectNode);
  const setDetailFocus = useStore((s) => s.setDetailFocus);
  const setActiveLens = useStore((s) => s.setActiveLens);
  const addSeed = useStore((s) => s.addSeed);
  const setGapItems = useStore((s) => s.setGapItems);
  const toggleGapPanel = useStore((s) => s.toggleGapPanel);
  const gapActive = useStore((s) => s.gapActive);
  const openLlmBar = useStore((s) => s.openLlmBar);
  const setLlmQuery = useStore((s) => s.setLlmQuery);
  const notify = useStore((s) => s.notify);
  const setPhraseIntent = useStore((s) => s.setPhraseIntent);
  const setPhraseResolution = useStore((s) => s.setPhraseResolution);
  const setPhraseResolving = useStore((s) => s.setPhraseResolving);
  const setPhraseError = useStore((s) => s.setPhraseError);
  const clearSearch = useStore((s) => s.clearSearch);
  const phraseRequestIdRef = useRef(0);
  const phraseExecutionIdRef = useRef(0);

  const symbols = searchResults.slice(0, 5);
  const notes = brainSearchResults.slice(0, 3);
  const hasResults = symbols.length > 0 || notes.length > 0;
  const normalizedQuery = searchQuery.trim().toLowerCase();
  const parsedPhrase = useMemo(() => parseSearchPhrase(searchQuery), [searchQuery]);
  const askText = searchQuery.replace(/^ask\s+/i, "").trim();

  useEffect(() => {
    setPhraseIntent(parsedPhrase);
  }, [parsedPhrase, setPhraseIntent]);

  useEffect(() => {
    phraseExecutionIdRef.current += 1;
  }, [searchQuery]);

  useEffect(() => {
    const requestId = ++phraseRequestIdRef.current;
    if (!phraseIntent) {
      setPhraseResolution(null);
      setPhraseResolving(false);
      return;
    }

    setPhraseResolving(true);
    resolveSearchPhrase(phraseIntent, {
      activeWorkspaceId,
      workspaces,
      symbolResults: searchResults,
      brainResults: brainSearchResults,
    })
      .then((resolution) => {
        if (requestId !== phraseRequestIdRef.current) return;
        setPhraseResolution(resolution);
      })
      .catch((error) => {
        if (requestId !== phraseRequestIdRef.current) return;
        setPhraseError(
          error instanceof Error && error.message
            ? error.message
            : "Phrase resolution failed",
        );
      });
  }, [
    activeWorkspaceId,
    brainSearchResults,
    phraseIntent,
    searchResults,
    setPhraseError,
    setPhraseResolution,
    setPhraseResolving,
    workspaces,
  ]);

  async function showGaps() {
    try {
      const items = await loadGapItems();
      setGapItems(items);
      if (!gapActive) toggleGapPanel();
    } catch (error) {
      notify({
        kind: "error",
        title: "Gap analysis failed",
        message:
          error instanceof Error && error.message
            ? error.message
            : "Gap analysis request failed",
      });
    }
  }

  function openDetail(uid: string, kind: string) {
    selectNode(uid, kind);
    setDetailFocus("summary");
    setActiveLens({
      lens: "search",
      label: "Search results",
      targetUid: uid,
      workspaceId: activeWorkspaceId,
    });
  }

  function addResultToScene(uid: string, kind: string) {
    selectNode(uid, kind);
    addSeed(uid);
    setActiveLens({
      lens: "search",
      label: "Search results",
      targetUid: uid,
      workspaceId: activeWorkspaceId,
    });
  }

  function askFromSearch() {
    if (!askText) return;
    setLlmQuery(askText);
    openLlmBar();
  }

  async function runPhrase(
    candidate?: PhraseCandidate,
    candidateOverrides?: PhraseCandidateOverrides,
  ) {
    if (!phraseResolution) return;
    const executionId = ++phraseExecutionIdRef.current;
    const isCurrent = () => executionId === phraseExecutionIdRef.current;
    try {
      const result = await executeSearchPhrase(useStore.getState(), phraseResolution, {
        targetOverride: candidate,
        targetOverrides: candidateOverrides,
        isCurrent,
        getCurrentState: useStore.getState,
      });
      if (!isCurrent() || result.status === "cancelled") return;
      notify({
        kind: result.status === "error" ? "error" : "info",
        title: result.status === "unsupported" ? "Phrase unsupported" : "Phrase executed",
        message: result.message,
      });
      if (result.status !== "unsupported" && result.status !== "error") {
        clearSearch();
      }
    } catch (error) {
      notify({
        kind: "error",
        title: "Phrase failed",
        message:
          error instanceof Error && error.message
            ? error.message
            : "Search phrase execution failed",
      });
    }
  }

  return (
    <GlassPanel
      role="listbox"
      aria-label="Search results"
      aria-activedescendant={activeDescendant}
      className="absolute top-full left-0 right-0 z-50 mt-1 max-h-80 overflow-y-auto rounded-md border border-[var(--color-border)] bg-[var(--color-surface)] shadow-lg"
    >
      {searchLoading && (
        <div className="px-3 py-2 text-sm text-[var(--color-text-muted)]">
          Searching...
        </div>
      )}

      {phraseIntent && (
        <PhrasePreview
          resolution={phraseResolution}
          resolving={phraseResolving}
          onExecute={(candidateOverrides) => runPhrase(undefined, candidateOverrides)}
          onCandidateExecute={(candidate) => runPhrase(candidate)}
        />
      )}

      {phraseError && (
        <div className="border-b border-[var(--color-border)] px-3 py-2 text-sm text-[var(--color-danger)]">
          {phraseError}
        </div>
      )}

      {!searchLoading && !hasResults && !phraseIntent && (
        <div className="px-3 py-2 text-sm text-[var(--color-text-muted)]">
          No results
        </div>
      )}

      {!phraseIntent && !searchLoading && normalizedQuery === "show gaps" && (
        <button
          type="button"
          onClick={showGaps}
          className="flex w-full items-center justify-between px-3 py-2 text-left text-sm hover:bg-[var(--color-surface-alt)]"
        >
          <span>
            <span className="font-medium">Show gaps</span>
            <span className="ml-2 text-xs text-[var(--color-text-muted)]">
              Open structural gap analysis
            </span>
          </span>
          <span className="text-xs text-[var(--color-graph-selection)]">Run</span>
        </button>
      )}

      {!phraseIntent && !searchLoading && normalizedQuery.startsWith("ask ") && (
        <button
          type="button"
          onClick={askFromSearch}
          disabled={!askText}
          className="flex w-full items-center justify-between px-3 py-2 text-left text-sm hover:bg-[var(--color-surface-alt)] disabled:cursor-not-allowed disabled:opacity-50"
        >
          <span>
            <span className="font-medium">Ask</span>
            <span className="ml-2 text-xs text-[var(--color-text-muted)]">
              {askText}
            </span>
          </span>
          <span className="text-xs text-[var(--color-graph-selection)]">Open</span>
        </button>
      )}

      {symbols.length > 0 && (
        <div>
          <div className="px-3 py-1.5 text-[10px] font-semibold uppercase tracking-wide text-[var(--color-text-muted)]">
            Symbols
          </div>
          {symbols.map((s) => (
            <div
              key={s.uid}
              id={`search-option-${s.uid}`}
              role="option"
              tabIndex={0}
              aria-selected={activeDescendant === `search-option-${s.uid}`}
              onClick={() => onSelect(s.uid, s.kind)}
              onKeyDown={(e) => { if (e.key === "Enter" || e.key === " ") { e.preventDefault(); onSelect(s.uid, s.kind); } }}
              className="flex w-full items-center gap-2 px-3 py-1.5 text-sm hover:bg-[var(--color-surface-alt)]"
            >
              <button
                type="button"
                onClick={(event) => {
                  event.stopPropagation();
                  onSelect(s.uid, s.kind);
                }}
                className="flex min-w-0 flex-1 items-center gap-2 text-left"
              >
                <KindBadge kind={s.kind} />
                <span className="min-w-0 truncate font-medium" title={s.name}>
                  {s.name}
                </span>
                <span
                  className="ml-auto hidden max-w-[38%] shrink-0 truncate text-xs text-[var(--color-text-muted)] md:inline"
                  title={s.file_path}
                >
                  {s.file_path}
                </span>
              </button>
              <span className="ml-2 flex shrink-0 gap-1">
                <button
                  type="button"
                  onClick={(event) => {
                    event.stopPropagation();
                    openDetail(s.uid, s.kind);
                  }}
                  className="rounded border border-[var(--color-border)] px-1.5 py-0.5 text-[10px] text-[var(--color-text-muted)] hover:text-[var(--color-text)]"
                >
                  Detail
                </button>
                <button
                  type="button"
                  onClick={(event) => {
                    event.stopPropagation();
                    onSelect(s.uid, s.kind);
                  }}
                  className="rounded border border-[var(--color-border)] px-1.5 py-0.5 text-[10px] text-[var(--color-text-muted)] hover:text-[var(--color-text)]"
                >
                  Explore
                </button>
                <button
                  type="button"
                  onClick={(event) => {
                    event.stopPropagation();
                    addResultToScene(s.uid, s.kind);
                  }}
                  className="rounded border border-[var(--color-border)] px-1.5 py-0.5 text-[10px] text-[var(--color-text-muted)] hover:text-[var(--color-text)]"
                >
                  Add
                </button>
              </span>
            </div>
          ))}
        </div>
      )}

      {notes.length > 0 && (
        <div>
          <div className="px-3 py-1.5 text-[10px] font-semibold uppercase tracking-wide text-[var(--color-text-muted)]">
            Notes &amp; Tags
          </div>
          {notes.map((n) => (
            <div
              key={n.uid}
              id={`search-option-${n.uid}`}
              role="option"
              tabIndex={0}
              aria-selected={activeDescendant === `search-option-${n.uid}`}
              onClick={() => onSelect(n.uid, n.kind)}
              onKeyDown={(e) => { if (e.key === "Enter" || e.key === " ") { e.preventDefault(); onSelect(n.uid, n.kind); } }}
              className="flex w-full items-center gap-2 px-3 py-1.5 text-sm hover:bg-[var(--color-surface-alt)]"
            >
              <button
                type="button"
                onClick={(event) => {
                  event.stopPropagation();
                  onSelect(n.uid, n.kind);
                }}
                className="flex min-w-0 flex-1 items-center gap-2 text-left"
              >
                <KindBadge kind={n.kind} />
                <span className="min-w-0 truncate font-medium" title={n.title}>
                  {n.title}
                </span>
                <span className="ml-auto shrink-0 text-xs text-[var(--color-text-muted)]">
                  {n.score.toFixed(2)}
                </span>
              </button>
              <span className="ml-2 flex shrink-0 gap-1">
                <button
                  type="button"
                  onClick={(event) => {
                    event.stopPropagation();
                    openDetail(n.uid, n.kind);
                  }}
                  className="rounded border border-[var(--color-border)] px-1.5 py-0.5 text-[10px] text-[var(--color-text-muted)] hover:text-[var(--color-text)]"
                >
                  Detail
                </button>
                <button
                  type="button"
                  onClick={(event) => {
                    event.stopPropagation();
                    onSelect(n.uid, n.kind);
                  }}
                  className="rounded border border-[var(--color-border)] px-1.5 py-0.5 text-[10px] text-[var(--color-text-muted)] hover:text-[var(--color-text)]"
                >
                  Explore
                </button>
                <button
                  type="button"
                  onClick={(event) => {
                    event.stopPropagation();
                    addResultToScene(n.uid, n.kind);
                  }}
                  className="rounded border border-[var(--color-border)] px-1.5 py-0.5 text-[10px] text-[var(--color-text-muted)] hover:text-[var(--color-text)]"
                >
                  Add
                </button>
              </span>
            </div>
          ))}
        </div>
      )}
    </GlassPanel>
  );
}
