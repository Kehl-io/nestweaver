import { useEffect, useState } from "react";
import { loadReposByUid, repoDisplayName } from "../../api/source";
import type { Repo } from "../../api/types";

interface RepoPickerProps {
  /** The path several indexed repos share. */
  filePath: string;
  /** Repo uids that index `filePath` (the 409 `ambiguous_file` candidates). */
  candidates: string[];
  /** The repo currently shown, if the user already picked one. */
  selected?: string;
  onPick: (repoUid: string) => void;
}

/**
 * nw-683: a file path indexed by more than one repo cannot be previewed
 * without naming the repo. Lists the candidate repos as buttons so the user
 * picks which copy to show.
 */
export function RepoPicker({ filePath, candidates, selected, onPick }: RepoPickerProps) {
  const [repos, setRepos] = useState<Map<string, Repo> | null>(null);

  useEffect(() => {
    let cancelled = false;
    loadReposByUid()
      .then((byUid) => {
        if (!cancelled) setRepos(byUid);
      })
      .catch(() => {
        // Names are cosmetic: without them the buttons show the uid's last segment.
      });
    return () => {
      cancelled = true;
    };
  }, []);

  return (
    <div className="rounded border border-[var(--color-border)] bg-[var(--color-surface-alt)] px-3 py-2 text-xs text-[var(--color-text-muted)]">
      <p className="mb-2">
        {filePath} exists in {candidates.length} indexed repos. Choose one to show:
      </p>
      <div role="group" aria-label={`Repos containing ${filePath}`} className="flex flex-wrap gap-1.5">
        {candidates.map((uid) => {
          const name = repoDisplayName(uid, repos);
          const isSelected = uid === selected;
          return (
            <button
              key={uid}
              type="button"
              title={uid}
              aria-label={`Show ${filePath} from ${name}`}
              aria-pressed={isSelected}
              onClick={() => onPick(uid)}
              className={`rounded border px-2 py-0.5 text-xs ${
                isSelected
                  ? "border-[var(--color-graph-selection)] bg-[var(--color-graph-selection)]/10 text-[var(--color-text)]"
                  : "border-[var(--color-border)] text-[var(--color-text)] hover:bg-[var(--color-surface)]"
              }`}
            >
              {name}
            </button>
          );
        })}
      </div>
    </div>
  );
}
