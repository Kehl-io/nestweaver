import { useEffect, useState } from "react";
import { api } from "../api/client";
import { useStore } from "../stores";
import { useLiveUpdates } from "../sse/useLiveUpdates";
import type { BrainStatus, Repo } from "../api/types";

export function StatusBar() {
  useLiveUpdates();

  const [status, setStatus] = useState<BrainStatus | null>(null);
  const [repos, setRepos] = useState<Repo[]>([]);
  const mode = useStore((s) => s.graphMode);
  const sseConnected = useStore((s) => s.sseConnected);

  useEffect(() => {
    api.brainStatus().then(setStatus).catch(() => {});
    api.repos().then(setRepos).catch(() => {});
  }, []);

  return (
    <footer className="flex h-6 shrink-0 items-center gap-4 border-t border-[var(--color-border)] px-4 text-xs text-[var(--color-text-muted)]">
      <span>{repos.length} repo{repos.length !== 1 ? "s" : ""}</span>
      {status && (
        <>
          <span>{status.vault_count} vault{status.vault_count !== 1 ? "s" : ""}</span>
          <span>{status.note_count} note{status.note_count !== 1 ? "s" : ""}</span>
        </>
      )}
      <span
        className={
          sseConnected
            ? "text-green-500"
            : "text-[var(--color-text-muted)]"
        }
      >
        {sseConnected ? "● Live" : "○ Static"}
      </span>
      <span className="ml-auto capitalize">{mode}</span>
    </footer>
  );
}
