import { useStore } from "../stores";
import { KindBadge } from "./shared/KindBadge";

interface SearchDropdownProps {
  onSelect: (uid: string, kind: string) => void;
}

export function SearchDropdown({ onSelect }: SearchDropdownProps) {
  const searchLoading = useStore((s) => s.searchLoading);
  const searchResults = useStore((s) => s.searchResults);
  const brainSearchResults = useStore((s) => s.brainSearchResults);

  const symbols = searchResults.slice(0, 5);
  const notes = brainSearchResults.slice(0, 3);
  const hasResults = symbols.length > 0 || notes.length > 0;

  return (
    <div className="absolute top-full left-0 right-0 z-50 mt-1 max-h-80 overflow-y-auto rounded-md border border-[var(--color-border)] bg-[var(--color-surface)] shadow-lg">
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

      {symbols.length > 0 && (
        <div>
          <div className="px-3 py-1.5 text-[10px] font-semibold uppercase tracking-wide text-[var(--color-text-muted)]">
            Symbols
          </div>
          {symbols.map((s) => (
            <button
              key={s.uid}
              type="button"
              onClick={() => onSelect(s.uid, s.kind)}
              className="flex w-full items-center gap-2 px-3 py-1.5 text-left text-sm hover:bg-[var(--color-surface-alt)]"
            >
              <KindBadge kind={s.kind} />
              <span className="font-medium">{s.name}</span>
              <span className="ml-auto truncate text-xs text-[var(--color-text-muted)]">
                {s.file_path}
              </span>
            </button>
          ))}
        </div>
      )}

      {notes.length > 0 && (
        <div>
          <div className="px-3 py-1.5 text-[10px] font-semibold uppercase tracking-wide text-[var(--color-text-muted)]">
            Notes &amp; Tags
          </div>
          {notes.map((n) => (
            <button
              key={n.uid}
              type="button"
              onClick={() => onSelect(n.uid, n.kind)}
              className="flex w-full items-center gap-2 px-3 py-1.5 text-left text-sm hover:bg-[var(--color-surface-alt)]"
            >
              <KindBadge kind={n.kind} />
              <span className="font-medium">{n.title}</span>
              <span className="ml-auto text-xs text-[var(--color-text-muted)]">
                {n.score.toFixed(2)}
              </span>
            </button>
          ))}
        </div>
      )}
    </div>
  );
}
