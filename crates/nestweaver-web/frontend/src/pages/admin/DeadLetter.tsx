import { useEffect, useState } from "react";
import { useAdminApi } from "../../hooks/useAdminApi";

interface DeadLetterEntry {
  id: string;
  repo: string;
  error: string;
  last_attempt: string;
  attempts: number;
}

export function DeadLetter() {
  const api = useAdminApi();
  const [entries, setEntries] = useState<DeadLetterEntry[]>([]);
  const [error, setError] = useState("");
  const [expanded, setExpanded] = useState<Set<string>>(new Set());

  function load() {
    api
      .get<DeadLetterEntry[]>("/dead-letter")
      .then(setEntries)
      .catch((e) => setError(e.message));
  }

  useEffect(() => {
    load();
  }, [api]);

  async function handleRetry(id: string) {
    try {
      await api.post(`/dead-letter/${encodeURIComponent(id)}/retry`);
      load();
    } catch (err) {
      setError(err instanceof Error ? err.message : "Failed to retry");
    }
  }

  async function handleDismiss(id: string) {
    try {
      await api.del(`/dead-letter/${encodeURIComponent(id)}`);
      load();
    } catch (err) {
      setError(err instanceof Error ? err.message : "Failed to dismiss");
    }
  }

  function toggleExpand(id: string) {
    setExpanded((prev) => {
      const next = new Set(prev);
      if (next.has(id)) next.delete(id);
      else next.add(id);
      return next;
    });
  }

  return (
    <div className="space-y-6">
      <h2 className="text-lg font-semibold text-[var(--color-text)]">
        Dead Letter Queue
      </h2>

      {error && (
        <div className="rounded-lg border border-red-300 bg-red-50 p-3 text-sm text-red-700 dark:border-red-800 dark:bg-red-900/20 dark:text-red-400">
          {error}
        </div>
      )}

      {entries.length === 0 && !error ? (
        <div className="rounded-lg border border-[var(--color-border)] bg-[var(--color-surface-alt)] p-8 text-center text-[var(--color-text-muted)]">
          No dead-letter entries.
        </div>
      ) : (
        <div className="overflow-x-auto rounded-lg border border-[var(--color-border)]">
          <table className="w-full text-sm">
            <thead>
              <tr className="border-b border-[var(--color-border)] bg-[var(--color-surface-alt)]">
                <th className="px-4 py-2 text-left font-medium text-[var(--color-text-muted)]">
                  Repo
                </th>
                <th className="px-4 py-2 text-left font-medium text-[var(--color-text-muted)]">
                  Error
                </th>
                <th className="px-4 py-2 text-left font-medium text-[var(--color-text-muted)]">
                  Last Attempt
                </th>
                <th className="px-4 py-2 text-left font-medium text-[var(--color-text-muted)]">
                  Attempts
                </th>
                <th className="px-4 py-2 text-right font-medium text-[var(--color-text-muted)]">
                  Actions
                </th>
              </tr>
            </thead>
            <tbody>
              {entries.map((entry) => (
                <tr
                  key={entry.id}
                  className="border-b border-[var(--color-border)] last:border-0"
                >
                  <td className="px-4 py-2 font-medium text-[var(--color-text)]">
                    {entry.repo}
                  </td>
                  <td className="max-w-xs px-4 py-2 text-[var(--color-text-muted)]">
                    <button
                      onClick={() => toggleExpand(entry.id)}
                      className="text-left hover:text-[var(--color-text)]"
                    >
                      {expanded.has(entry.id)
                        ? entry.error
                        : entry.error.length > 80
                          ? entry.error.slice(0, 80) + "..."
                          : entry.error}
                    </button>
                  </td>
                  <td className="px-4 py-2 text-[var(--color-text-muted)]">
                    {entry.last_attempt}
                  </td>
                  <td className="px-4 py-2 text-[var(--color-text-muted)]">
                    {entry.attempts}
                  </td>
                  <td className="px-4 py-2 text-right">
                    <button
                      onClick={() => handleRetry(entry.id)}
                      className="mr-2 rounded px-2 py-1 text-xs text-[var(--color-graph-selection)] hover:bg-[var(--color-surface-alt)]"
                    >
                      Retry
                    </button>
                    <button
                      onClick={() => handleDismiss(entry.id)}
                      className="rounded px-2 py-1 text-xs text-red-500 hover:bg-red-50 dark:hover:bg-red-900/20"
                    >
                      Dismiss
                    </button>
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}
    </div>
  );
}
