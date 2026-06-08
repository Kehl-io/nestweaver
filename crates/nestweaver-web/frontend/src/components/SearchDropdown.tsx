import { useStore } from "../stores";
import { api } from "../api/client";
import { GlassPanel } from "./panels/GlassPanel";
import { KindBadge } from "./shared/KindBadge";
import type { GapItem } from "../stores/analysisSlice";

interface SearchDropdownProps {
  onSelect: (uid: string, kind: string) => void;
  activeDescendant?: string;
}

export function SearchDropdown({ onSelect, activeDescendant }: SearchDropdownProps) {
  const searchQuery = useStore((s) => s.searchQuery);
  const searchLoading = useStore((s) => s.searchLoading);
  const searchResults = useStore((s) => s.searchResults);
  const brainSearchResults = useStore((s) => s.brainSearchResults);
  const selectNode = useStore((s) => s.selectNode);
  const setDetailFocus = useStore((s) => s.setDetailFocus);
  const addSeed = useStore((s) => s.addSeed);
  const setGraphMode = useStore((s) => s.setGraphMode);
  const setGapItems = useStore((s) => s.setGapItems);
  const toggleGapPanel = useStore((s) => s.toggleGapPanel);
  const gapActive = useStore((s) => s.gapActive);
  const openLlmBar = useStore((s) => s.openLlmBar);
  const setLlmQuery = useStore((s) => s.setLlmQuery);

  const symbols = searchResults.slice(0, 5);
  const notes = brainSearchResults.slice(0, 3);
  const hasResults = symbols.length > 0 || notes.length > 0;
  const normalizedQuery = searchQuery.trim().toLowerCase();
  const askText = searchQuery.replace(/^ask\s+/i, "").trim();
  const impactText = searchQuery.replace(/^impact of\s+/i, "").trim();

  async function showGaps() {
    const report = await api.gaps();
    const items: GapItem[] = [
      ...report.undocumented.map((m) => ({
        type: "undocumented" as const,
        label: m.module,
        detail: `${m.symbol_count} symbols with no documentation`,
        nodeUids: [] as string[],
      })),
      ...report.untested.map((uid) => ({
        type: "untested" as const,
        label: uid.split(":").pop() || uid,
        detail: "Entry point with no test coverage",
        nodeUids: [uid],
      })),
    ];
    setGapItems(items);
    if (!gapActive) toggleGapPanel();
  }

  function openDetail(uid: string, kind: string) {
    selectNode(uid, kind);
    setDetailFocus("summary");
  }

  function addResultToScene(uid: string, kind: string) {
    selectNode(uid, kind);
    addSeed(uid);
  }

  function impactFirstResult() {
    const first = symbols[0];
    if (!first) return;
    selectNode(first.uid, first.kind);
    setDetailFocus("analysis");
    setGraphMode("impact");
  }

  function askFromSearch() {
    if (!askText) return;
    setLlmQuery(askText);
    openLlmBar();
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

      {!searchLoading && !hasResults && (
        <div className="px-3 py-2 text-sm text-[var(--color-text-muted)]">
          No results
        </div>
      )}

      {!searchLoading && normalizedQuery === "show gaps" && (
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
          <span className="text-xs text-blue-600">Run</span>
        </button>
      )}

      {!searchLoading && normalizedQuery.startsWith("impact of ") && (
        <button
          type="button"
          onClick={impactFirstResult}
          disabled={!symbols[0]}
          className="flex w-full items-center justify-between px-3 py-2 text-left text-sm hover:bg-[var(--color-surface-alt)] disabled:cursor-not-allowed disabled:opacity-50"
        >
          <span>
            <span className="font-medium">Impact of {impactText}</span>
            <span className="ml-2 text-xs text-[var(--color-text-muted)]">
              Use the top symbol result
            </span>
          </span>
          <span className="text-xs text-blue-600">Analyze</span>
        </button>
      )}

      {!searchLoading && normalizedQuery.startsWith("ask ") && (
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
          <span className="text-xs text-blue-600">Open</span>
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
              aria-selected={activeDescendant === `search-option-${s.uid}`}
              onClick={() => onSelect(s.uid, s.kind)}
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
                <span className="min-w-0 font-medium">{s.name}</span>
                <span className="ml-auto truncate text-xs text-[var(--color-text-muted)]">
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
              aria-selected={activeDescendant === `search-option-${n.uid}`}
              onClick={() => onSelect(n.uid, n.kind)}
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
                <span className="min-w-0 font-medium">{n.title}</span>
                <span className="ml-auto text-xs text-[var(--color-text-muted)]">
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
