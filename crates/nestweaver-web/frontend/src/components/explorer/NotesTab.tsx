import { useEffect, useMemo, useState } from "react";
import { api, NOTES_MAX_REACHABLE } from "../../api/client";
import type { Note, Tag, Vault } from "../../api/types";
import { useStore } from "../../stores";
import { Collapsible } from "../shared/Collapsible";

const KIND_BADGE: Record<
  string,
  { label: string; className: string }
> = {
  General: {
    label: "General",
    className: "bg-gray-200 text-gray-700 dark:bg-gray-700 dark:text-gray-300",
  },
  PRD: {
    label: "PRD",
    className: "bg-blue-100 text-blue-700 dark:bg-blue-900 dark:text-blue-300",
  },
  Design: {
    label: "Design",
    className:
      "bg-purple-100 text-purple-700 dark:bg-purple-900 dark:text-purple-300",
  },
  Meeting: {
    label: "Meeting",
    className:
      "bg-green-100 text-green-700 dark:bg-green-900 dark:text-green-300",
  },
  Journal: {
    label: "Journal",
    className:
      "bg-amber-100 text-amber-700 dark:bg-amber-900 dark:text-amber-300",
  },
};

function KindBadge({ kind }: { kind: string }) {
  const badge = KIND_BADGE[kind] ?? {
    label: kind,
    className:
      "bg-gray-100 text-gray-600 dark:bg-gray-800 dark:text-gray-400",
  };
  return (
    <span
      className={`shrink-0 rounded px-1.5 py-0.5 text-[10px] font-medium leading-none ${badge.className}`}
    >
      {badge.label}
    </span>
  );
}

/** Loaded notes for one vault plus its true size (nw-648). */
interface VaultNotes {
  notes: Note[];
  /** The vault's real note count: vault inventory first, page header second. */
  total: number;
  loadingMore: boolean;
  error: string | null;
}

export function NotesTab() {
  const exploreNode = useStore((s) => s.exploreNode);
  const selectNode = useStore((s) => s.selectNode);

  const [vaults, setVaults] = useState<Vault[]>([]);
  const [byVault, setByVault] = useState<Record<string, VaultNotes>>({});
  const [tags, setTags] = useState<Tag[]>([]);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [filter, setFilter] = useState("");

  // nw-648: one unfiltered 1000-row page held only the first vault, so the
  // explorer showed "BRAIN 991" while the brain had two vaults and ~1918
  // notes. Every vault is now listed from the inventory with its TRUE count,
  // and each loads its own notes through the vault filter.
  useEffect(() => {
    let cancelled = false;
    setLoading(true);
    setError(null);
    Promise.all([api.brainVaults(), api.brainTags()])
      .then(async ([v, t]) => {
        const pages = await Promise.all(
          v.map((vault) => api.brainNotesPage(vault.uid)),
        );
        if (cancelled) return;
        const loaded: Record<string, VaultNotes> = {};
        v.forEach((vault, i) => {
          const page = pages[i];
          loaded[vault.uid] = {
            notes: page.notes,
            total: vault.note_count ?? page.total ?? page.notes.length,
            loadingMore: false,
            error: null,
          };
        });
        setVaults(v);
        setByVault(loaded);
        setTags(t);
      })
      .catch((e) => {
        if (!cancelled) setError(e.message ?? "Failed to load notes");
      })
      .finally(() => {
        if (!cancelled) setLoading(false);
      });
    return () => {
      cancelled = true;
    };
  }, []);

  const loadMore = (vaultUid: string) => {
    const current = byVault[vaultUid];
    if (!current || current.loadingMore) return;
    setByVault((prev) => ({
      ...prev,
      [vaultUid]: { ...prev[vaultUid], loadingMore: true, error: null },
    }));
    api
      .brainNotesPage(vaultUid, current.notes.length)
      .then((page) =>
        setByVault((prev) => ({
          ...prev,
          [vaultUid]: {
            ...prev[vaultUid],
            notes: [...prev[vaultUid].notes, ...page.notes],
            loadingMore: false,
          },
        })),
      )
      .catch((e) =>
        setByVault((prev) => ({
          ...prev,
          [vaultUid]: {
            ...prev[vaultUid],
            loadingMore: false,
            error: e.message ?? "Failed to load more notes",
          },
        })),
      );
  };

  const notesByVault = useMemo(() => {
    const lc = filter.toLowerCase();
    const map = new Map<string, Note[]>();
    for (const [vaultUid, entry] of Object.entries(byVault)) {
      map.set(
        vaultUid,
        entry.notes.filter((n) => {
          if (n.title.includes("{{") || n.file_path.includes("_templates/")) return false;
          if (lc && !n.title.toLowerCase().includes(lc)) return false;
          return true;
        }),
      );
    }
    return map;
  }, [byVault, filter]);

  const tagsByVault = useMemo(() => {
    const map = new Map<string, Tag[]>();
    for (const t of tags) {
      if (!map.has(t.vault_uid)) map.set(t.vault_uid, []);
      map.get(t.vault_uid)!.push(t);
    }
    return map;
  }, [tags]);

  const totalNotes = Object.values(byVault).reduce((sum, v) => sum + v.total, 0);

  if (loading) {
    return (
      <div className="flex h-full items-center justify-center text-sm text-[var(--color-text-muted)]">
        Loading...
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

  if (vaults.length === 0 || totalNotes === 0) {
    return (
      <div className="flex h-full items-center justify-center p-4 text-sm text-[var(--color-text-muted)]">
        No notes indexed yet.
      </div>
    );
  }

  return (
    <div className="flex h-full flex-col overflow-hidden">
      {/* Search input */}
      <div className="border-b border-[var(--color-border)] p-2">
        <input
          type="text"
          placeholder="Filter notes..."
          value={filter}
          onChange={(e) => setFilter(e.target.value)}
          className="w-full rounded border border-[var(--color-border)] bg-[var(--color-surface-alt)] px-2 py-1 text-xs text-[var(--color-text)] placeholder:text-[var(--color-text-muted)] focus:outline-none focus:ring-1 focus:ring-blue-500"
        />
      </div>

      <div className="flex-1 overflow-y-auto">
        {/* Notes grouped by vault */}
        {vaults.map((vault) => {
          const vaultNotes = notesByVault.get(vault.uid) ?? [];
          const entry = byVault[vault.uid];
          const loaded = entry?.notes.length ?? 0;
          const total = entry?.total ?? loaded;
          const unlisted = Math.max(0, total - loaded);
          const canLoadMore =
            unlisted > 0 && loaded > 0 && loaded < NOTES_MAX_REACHABLE;
          return (
            <Collapsible
              key={vault.uid}
              title={vault.name}
              count={total}
              defaultOpen
            >
              <div className="pb-1" data-testid={`notes-vault-${vault.name}`}>
                {vaultNotes.length === 0 ? (
                  <div className="px-4 py-1 text-[10px] text-[var(--color-text-muted)]">
                    No matching notes.
                  </div>
                ) : (
                  <ul>
                    {vaultNotes.map((note) => (
                      <li key={note.uid}>
                        <button
                          type="button"
                          onClick={() => {
                            exploreNode(note.uid, "note");
                          }}
                          className="flex w-full items-center gap-1.5 border-b border-[var(--color-border)] px-4 py-1.5 text-left hover:bg-[var(--color-surface-alt)]"
                        >
                          <span className="min-w-0 flex-1 truncate text-xs text-[var(--color-text)]">
                            {note.title}
                            {note.file_path && (
                              <span className="ml-1 text-[10px] text-[var(--color-text-muted)]">
                                {note.file_path.split("/").slice(-2, -1)[0] || ""}
                              </span>
                            )}
                          </span>
                          <KindBadge kind={note.note_kind} />
                          {note.word_count > 0 && (
                            <span className="shrink-0 text-[10px] text-[var(--color-text-muted)]">
                              {note.word_count}w
                            </span>
                          )}
                        </button>
                      </li>
                    ))}
                  </ul>
                )}
                {unlisted > 0 && (
                  <div
                    className="flex items-center gap-2 px-4 py-1 text-[10px] text-[var(--color-text-muted)]"
                    data-testid="notes-vault-truncated"
                  >
                    <span>
                      Showing {loaded} of {total} notes
                      {canLoadMore ? "." : " — use search to reach the rest."}
                    </span>
                    {canLoadMore && (
                      <button
                        type="button"
                        onClick={() => loadMore(vault.uid)}
                        disabled={entry?.loadingMore}
                        className="text-blue-600 hover:underline disabled:opacity-50"
                      >
                        {entry?.loadingMore ? "Loading..." : "Load more"}
                      </button>
                    )}
                  </div>
                )}
                {entry?.error && (
                  <div className="px-4 py-1 text-[10px] text-red-500">{entry.error}</div>
                )}
              </div>
            </Collapsible>
          );
        })}

        {/* Tags section — collapsed by default */}
        <Collapsible title="Tags" count={tags.length} defaultOpen={false}>
          <div className="pb-1">
            {vaults.map((vault) => {
              const vaultTags = tagsByVault.get(vault.uid) ?? [];
              if (vaultTags.length === 0) return null;
              return (
                <div key={vault.uid}>
                  <div className="px-4 py-0.5 text-[10px] font-semibold uppercase tracking-wide text-[var(--color-text-muted)]">
                    {vault.name}
                  </div>
                  <ul>
                    {vaultTags.map((tag) => (
                      <li key={tag.uid}>
                        <button
                          type="button"
                          onClick={() => selectNode(tag.uid, "tag")}
                          className="flex w-full items-center gap-1.5 px-5 py-1 text-left text-xs text-[var(--color-text)] hover:bg-[var(--color-surface-alt)]"
                        >
                          <span className="text-[var(--color-tag)]">#</span>
                          <span className="truncate">{tag.name}</span>
                        </button>
                      </li>
                    ))}
                  </ul>
                </div>
              );
            })}
          </div>
        </Collapsible>
      </div>
    </div>
  );
}
