import { useEffect, useRef } from "react";
import { useHotkeys } from "react-hotkeys-hook";
import { useDebouncedCallback } from "use-debounce";
import { api } from "../api/client";
import { useStore } from "../stores";
import type { ScopeFilter } from "../api/types";
import { PerspectiveSelector } from "./PerspectiveSelector";
import { SearchDropdown } from "./SearchDropdown";

const THEME_ICONS: Record<string, string> = {
  system: "◐",
  light: "☀",
  dark: "☾",
};

const THEME_CYCLE: Record<string, "system" | "light" | "dark"> = {
  system: "light",
  light: "dark",
  dark: "system",
};

function ThemeToggle() {
  const theme = useStore((s) => s.theme);
  const setTheme = useStore((s) => s.setTheme);

  return (
    <button
      onClick={() => setTheme(THEME_CYCLE[theme])}
      title={`Theme: ${theme} (click to cycle)`}
      className="flex h-8 w-8 shrink-0 items-center justify-center rounded border border-[var(--color-border)] bg-[var(--color-surface-alt)] text-sm hover:bg-[var(--color-border)]"
    >
      {THEME_ICONS[theme]}
    </button>
  );
}

export function TopBar() {
  const inputRef = useRef<HTMLInputElement>(null);

  const theme = useStore((s) => s.theme);
  const searchQuery = useStore((s) => s.searchQuery);
  const searchOpen = useStore((s) => s.searchOpen);
  const scopeFilter = useStore((s) => s.scopeFilter);
  const setSearchQuery = useStore((s) => s.setSearchQuery);
  const setSearchOpen = useStore((s) => s.setSearchOpen);
  const setSearchLoading = useStore((s) => s.setSearchLoading);
  const setSearchResults = useStore((s) => s.setSearchResults);
  const clearSearch = useStore((s) => s.clearSearch);
  const exploreNode = useStore((s) => s.exploreNode);
  const setScopeFilter = useStore((s) => s.setScopeFilter);

  const debouncedSearch = useDebouncedCallback(async (q: string) => {
    if (!q.trim()) {
      setSearchOpen(false);
      return;
    }
    setSearchLoading(true);
    try {
      const [symbols, brain] = await Promise.all([
        api.search(q, 10),
        api.brainSearch(q, 5),
      ]);
      setSearchResults(symbols, brain);
    } catch {
      setSearchResults([], []);
    } finally {
      setSearchLoading(false);
    }
  }, 200);

  function handleInputChange(e: React.ChangeEvent<HTMLInputElement>) {
    const q = e.target.value;
    setSearchQuery(q);
    setSearchOpen(true);
    debouncedSearch(q);
  }

  function handleSelect(uid: string, kind: string) {
    exploreNode(uid, kind);
    clearSearch();
    inputRef.current?.blur();
  }

  useHotkeys(
    "/",
    (e) => {
      e.preventDefault();
      inputRef.current?.focus();
    },
    { enableOnFormTags: false },
  );

  useEffect(() => {
    function handleGlobalSearchFocus(event: KeyboardEvent) {
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
  }, []);

  useHotkeys(
    "escape",
    () => {
      clearSearch();
      inputRef.current?.blur();
    },
    { enableOnFormTags: ["INPUT"] },
  );

  return (
    <header data-testid="top-bar" className="sticky top-0 z-50 flex h-12 shrink-0 items-center gap-2 overflow-visible border-b border-[var(--color-border)] bg-[var(--color-surface)] px-2 sm:gap-3 sm:px-4">
      <img
        src="/favicon.svg"
        alt="NestWeaver"
        className="h-8 w-8 shrink-0 sm:hidden"
      />
      <img
        src={theme === "dark" || (theme === "system" && window.matchMedia("(prefers-color-scheme: dark)").matches)
          ? "/logo-horizontal-dark.svg"
          : "/logo-horizontal-light.svg"}
        alt="NestWeaver"
        className="hidden h-8 shrink-0 sm:block"
      />

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

      <select
        value={scopeFilter}
        onChange={(e) => setScopeFilter(e.target.value as ScopeFilter)}
        className="w-[5.75rem] shrink-0 rounded border border-[var(--color-border)] bg-[var(--color-surface-alt)] px-2 py-1.5 text-xs outline-none"
      >
        <option value="all">All</option>
        <option value="code_only">Code only</option>
        <option value="notes_only">Notes only</option>
      </select>

      <PerspectiveSelector />
      <ThemeToggle />
    </header>
  );
}
