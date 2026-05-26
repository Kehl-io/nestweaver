import { useEffect, useMemo, useState } from "react";
import { api } from "../../api/client";
import type { Tag, Vault } from "../../api/types";
import { useStore } from "../../stores";
import { Collapsible } from "../shared/Collapsible";

export function NotesTab() {
  const selectNode = useStore((s) => s.selectNode);

  const [vaults, setVaults] = useState<Vault[]>([]);
  const [tags, setTags] = useState<Tag[]>([]);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [tagFilter, setTagFilter] = useState("");

  useEffect(() => {
    setLoading(true);
    setError(null);
    Promise.all([api.brainVaults(), api.brainTags()])
      .then(([v, t]) => {
        setVaults(v);
        setTags(t);
      })
      .catch((e) => setError(e.message ?? "Failed to load notes"))
      .finally(() => setLoading(false));
  }, []);

  const filteredTags = useMemo(() => {
    const lc = tagFilter.toLowerCase();
    if (!lc) return tags;
    return tags.filter((t) => t.name.toLowerCase().includes(lc));
  }, [tags, tagFilter]);

  const tagsByVault = useMemo(() => {
    const map = new Map<string, Tag[]>();
    for (const t of filteredTags) {
      if (!map.has(t.vault_uid)) map.set(t.vault_uid, []);
      map.get(t.vault_uid)!.push(t);
    }
    return map;
  }, [filteredTags]);

  if (loading) {
    return (
      <div className="flex h-full items-center justify-center text-sm text-[var(--color-text-muted)]">
        Loading notes...
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

  if (vaults.length === 0) {
    return (
      <div className="flex h-full items-center justify-center p-4 text-sm text-[var(--color-text-muted)]">
        No vaults indexed.
      </div>
    );
  }

  return (
    <div className="flex h-full flex-col overflow-hidden">
      <div className="border-b border-[var(--color-border)] p-2">
        <input
          type="text"
          placeholder="Filter tags..."
          value={tagFilter}
          onChange={(e) => setTagFilter(e.target.value)}
          className="w-full rounded border border-[var(--color-border)] bg-[var(--color-surface-alt)] px-2 py-1 text-xs text-[var(--color-text)] placeholder:text-[var(--color-text-muted)] focus:outline-none focus:ring-1 focus:ring-blue-500"
        />
      </div>

      <div className="flex-1 overflow-y-auto">
        {vaults.map((vault) => {
          const vaultTags = tagsByVault.get(vault.uid) ?? [];
          return (
            <Collapsible
              key={vault.uid}
              title={vault.name}
              count={vaultTags.length}
              defaultOpen
            >
              <div className="pb-1">
                {vaultTags.length === 0 ? (
                  <div className="px-4 py-1 text-[10px] text-[var(--color-text-muted)]">
                    No matching tags.
                  </div>
                ) : (
                  <ul>
                    {vaultTags.map((tag) => (
                      <li key={tag.uid}>
                        <button
                          type="button"
                          onClick={() => selectNode(tag.uid, "tag")}
                          className="flex w-full items-center gap-1.5 px-4 py-1 text-left text-xs text-[var(--color-text)] hover:bg-[var(--color-surface-alt)]"
                        >
                          <span className="text-[var(--color-tag)]">#</span>
                          <span className="truncate">{tag.name}</span>
                        </button>
                      </li>
                    ))}
                  </ul>
                )}
              </div>
            </Collapsible>
          );
        })}
      </div>
    </div>
  );
}
