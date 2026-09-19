import { useEffect, useRef, useState } from "react";
import { useHotkeys } from "react-hotkeys-hook";
import { useDebouncedCallback } from "use-debounce";
import { api } from "../api/client";
import type { ScopedSearchHit, ScopedSymbolSearchHit } from "../api/p1Types";
import type { SearchHit, SymbolCandidate } from "../api/types";
import { brainSearchInWorkspace } from "../api/workspaces";
import { useStore } from "../stores";
import { PerspectiveSelector } from "./PerspectiveSelector";
import { SearchDropdown } from "./SearchDropdown";
import { ScopeSelect } from "./shared/ScopeSelect";
import { ThemeMenu } from "./shared/ThemeMenu";
import { WorkspaceSwitcher } from "./workspace/WorkspaceSwitcher";

function getErrorMessage(error: unknown, fallback: string) {
  return error instanceof Error && error.message ? error.message : fallback;
}

function isScopedSymbolHit(hit: ScopedSearchHit): hit is ScopedSymbolSearchHit {
  return "repo_uid" in hit && "file_path" in hit;
}

function splitScopedSearchResults(results: ScopedSearchHit[]): {
  symbols: SymbolCandidate[];
  brain: SearchHit[];
} {
  return results.reduce(
    (acc, hit) => {
      if (isScopedSymbolHit(hit)) {
        acc.symbols.push({
          uid: hit.uid,
          name: hit.name || hit.title,
          kind: "symbol",
          file_path: hit.file_path,
          start_line: 0,
        });
      } else {
        acc.brain.push({
          uid: hit.uid,
          kind: hit.kind,
          title: hit.title,
          vault_uid: hit.vault_uid,
          score: hit.score,
        });
      }
      return acc;
    },
    { symbols: [] as SymbolCandidate[], brain: [] as SearchHit[] },
  );
}

export function TopBar() {
  const inputRef = useRef<HTMLInputElement>(null);
  const previousWorkspaceIdRef = useRef<string | null>(null);
  const searchGenerationRef = useRef(0);
  const [prefersDark, setPrefersDark] = useState(false);

  const theme = useStore((s) => s.theme);
  const activeWorkspaceId = useStore((s) => s.activeWorkspaceId);
  const searchQuery = useStore((s) => s.searchQuery);
  const searchOpen = useStore((s) => s.searchOpen);
  const scopeFilter = useStore((s) => s.scopeFilter);
  const modalOpen = useStore((s) => s.llmBarOpen || s.shortcutsOpen);
  const setSearchQuery = useStore((s) => s.setSearchQuery);
  const setSearchOpen = useStore((s) => s.setSearchOpen);
  const setSearchLoading = useStore((s) => s.setSearchLoading);
  const setSearchResults = useStore((s) => s.setSearchResults);
  const clearSearch = useStore((s) => s.clearSearch);
  const exploreNode = useStore((s) => s.exploreNode);
  const setActiveLens = useStore((s) => s.setActiveLens);
  const setScopeFilter = useStore((s) => s.setScopeFilter);

  function isCurrentSearch(
    generation: number,
    q: string,
    workspaceId: string | null,
    scope: typeof scopeFilter,
  ) {
    const state = useStore.getState();
    return (
      searchGenerationRef.current === generation &&
      state.searchQuery === q &&
      state.activeWorkspaceId === workspaceId &&
      state.scopeFilter === scope
    );
  }

  function beginSearch(q: string, workspaceId: string | null) {
    const generation = searchGenerationRef.current + 1;
    searchGenerationRef.current = generation;
    if (!q.trim()) {
      setSearchResults([], []);
      setSearchLoading(false);
      setSearchOpen(false);
      return;
    }
    setSearchOpen(true);
    setSearchLoading(true);
    debouncedSearch(q, workspaceId, scopeFilter, generation);
  }

  const debouncedSearch = useDebouncedCallback(async (
    q: string,
    workspaceId: string | null,
    scope: typeof scopeFilter,
    generation: number,
  ) => {
    if (!isCurrentSearch(generation, q, workspaceId, scope)) return;
    if (!q.trim()) {
      setSearchOpen(false);
      return;
    }
    setSearchLoading(true);
    try {
      if (workspaceId === "all") {
        const symbolsPromise =
          scope === "notes_only" ? Promise.resolve([]) : api.search(q, 10);
        const brainPromise =
          scope === "code_only" ? Promise.resolve([]) : api.brainSearch(q, 5);
        const [symbols, brain] = await Promise.all([symbolsPromise, brainPromise]);
        if (!isCurrentSearch(generation, q, workspaceId, scope)) return;
        setSearchResults(symbols, brain);
      } else {
        const scoped = await brainSearchInWorkspace(q, {
          workspaceId,
          limit: 15,
        });
        if (!isCurrentSearch(generation, q, workspaceId, scope)) return;
        const split = splitScopedSearchResults(scoped.results);
        const symbols = scope === "notes_only" ? [] : split.symbols.slice(0, 10);
        const brain = scope === "code_only" ? [] : split.brain.slice(0, 5);
        setSearchResults(symbols, brain);
      }
    } catch (error) {
      if (!isCurrentSearch(generation, q, workspaceId, scope)) return;
      useStore.getState().notify({
        kind: "error",
        title: "Search failed",
        message: getErrorMessage(error, "Search request failed"),
      });
      setSearchResults([], []);
    } finally {
      if (isCurrentSearch(generation, q, workspaceId, scope)) {
        setSearchLoading(false);
      }
    }
  }, 200);

  useEffect(() => {
    if (previousWorkspaceIdRef.current === activeWorkspaceId) return;
    previousWorkspaceIdRef.current = activeWorkspaceId;
    setSearchResults([], []);
    beginSearch(searchQuery, activeWorkspaceId);
  }, [activeWorkspaceId]);

  useEffect(() => {
    if (!searchQuery.trim()) return;
    beginSearch(searchQuery, activeWorkspaceId);
    // Scope changes must re-issue the same query against the matching APIs.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [scopeFilter]);

  function handleInputChange(e: React.ChangeEvent<HTMLInputElement>) {
    const q = e.target.value;
    setSearchQuery(q);
    beginSearch(q, activeWorkspaceId);
  }

  function handleSelect(uid: string, kind: string) {
    exploreNode(uid, kind);
    setActiveLens({
      lens: "search",
      label: "Search results",
      targetUid: uid,
      workspaceId: activeWorkspaceId,
    });
    clearSearch();
    inputRef.current?.blur();
  }

  useHotkeys(
    "/",
    (e) => {
      e.preventDefault();
      inputRef.current?.focus();
    },
    { enableOnFormTags: false, enabled: !modalOpen },
  );

  useEffect(() => {
    function handleGlobalSearchFocus(event: KeyboardEvent) {
      if (modalOpen) return;
      if (event.key !== "/") return;
      const target = event.target as HTMLElement | null;
      if (
        target?.tagName === "INPUT" ||
        target?.tagName === "TEXTAREA" ||
        target?.isContentEditable
      ) {
        return;
      }
      event.preventDefault();
      inputRef.current?.focus();
    }

    window.addEventListener("keydown", handleGlobalSearchFocus);
    return () => window.removeEventListener("keydown", handleGlobalSearchFocus);
  }, [modalOpen]);

  useHotkeys(
    "escape",
    () => {
      clearSearch();
      inputRef.current?.blur();
    },
    { enableOnFormTags: ["INPUT"], enabled: !modalOpen },
  );

  useEffect(() => {
    if (typeof window === "undefined" || !window.matchMedia) return;

    const mediaQuery = window.matchMedia("(prefers-color-scheme: dark)");
    const syncPreference = () => setPrefersDark(mediaQuery.matches);
    syncPreference();
    mediaQuery.addEventListener("change", syncPreference);
    return () => mediaQuery.removeEventListener("change", syncPreference);
  }, []);

  const darkLogo = theme === "dark" || (theme === "system" && prefersDark);

  return (
    <header data-testid="top-bar" className="sticky top-0 z-50 flex h-12 shrink-0 items-center gap-2 overflow-visible border-b border-[var(--color-border)] bg-[var(--color-surface)] px-2 sm:gap-3 sm:px-4">
      <img
        src={darkLogo ? "/logo-icon-dark.svg" : "/logo-icon-light.svg"}
        alt="NestWeaver"
        className="h-8 w-8 shrink-0 sm:hidden"
      />
      <img
        src={darkLogo ? "/logo-horizontal-dark.svg" : "/logo-horizontal-light.svg"}
        alt="NestWeaver"
        className="hidden h-8 shrink-0 sm:block"
      />

      <WorkspaceSwitcher />

      <div className="relative min-w-0 flex-1 sm:max-w-md">
        <input
          data-testid="search-input"
          ref={inputRef}
          type="text"
          value={searchQuery}
          onChange={handleInputChange}
          onFocus={() => {
            if (searchQuery.trim()) setSearchOpen(true);
          }}
          placeholder="Search"
          className="w-full rounded border border-[var(--color-border)] bg-[var(--color-surface-alt)] px-2 py-1.5 text-sm outline-none focus:border-[var(--color-graph-selection)] sm:px-3"
        />
        {searchOpen && <SearchDropdown onSelect={handleSelect} />}
      </div>

      <ScopeSelect
        value={scopeFilter}
        onChange={setScopeFilter}
        label="Search filter"
        compact
      />

      <PerspectiveSelector />
      <ThemeMenu />
    </header>
  );
}
