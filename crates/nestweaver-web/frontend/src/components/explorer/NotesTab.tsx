import { useEffect, useMemo, useState } from "react";
import { api } from "../../api/client";
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

export function NotesTab() {
  const exploreNode = useStore((s) => s.exploreNode);
  const selectNode = useStore((s) => s.selectNode);

  const [vaults, setVaults] = useState<Vault[]>([]);
  const [notes, setNotes] = useState<Note[]>([]);
  const [tags, setTags] = useState<Tag[]>([]);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [filter, setFilter] = useState("");

  useEffect(() => {
    setLoading(true);
    setError(null);
    Promise.all([api.brainVaults(), api.brainNotes(), api.brainTags()])
      .then(([v, n, t]) => {
        setVaults(v);
        setNotes(n);
        setTags(t);
      })
      .catch((e) => setError(e.message ?? "Failed to load notes"))
      .finally(() => setLoading(false));
  }, []);

  const filteredNotes = useMemo(() => {
    const lc = filter.toLowerCase();
    return notes.filter((n) => {
      if (n.title.includes("{{") || n.file_path.includes("_templates/")) return false;
      if (lc && !n.title.toLowerCase().includes(lc)) return false;
      return true;
    });
  }, [notes, filter]);

  const notesByVault = useMemo(() => {
    const map = new Map<string, Note[]>();
    for (const n of filteredNotes) {
      if (!map.has(n.vault_uid)) map.set(n.vault_uid, []);
      map.get(n.vault_uid)!.push(n);
    }
    return map;
  }, [filteredNotes]);

  const tagsByVault = useMemo(() => {
    const map = new Map<string, Tag[]>();
    for (const t of tags) {
      if (!map.has(t.vault_uid)) map.set(t.vault_uid, []);
      map.get(t.vault_uid)!.push(t);
    }
    return map;
  }, [tags]);

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

  if (vaults.length === 0 || notes.length === 0) {
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
          return (
            <Collapsible
              key={vault.uid}
              title={vault.name}
              count={vaultNotes.length}
              defaultOpen
            >
              <div className="pb-1">
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
