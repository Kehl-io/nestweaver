import { useEffect, useState, type FormEvent } from "react";
import { useAdminApi } from "../../hooks/useAdminApi";

interface Repo {
  id: string;
  url: string;
  status: string;
  indexed_sha?: string;
  freshness?: string;
  next_poll?: string;
  poll_mode?: string;
  symbol_count?: number;
}

const statusColors: Record<string, string> = {
  indexed:
    "bg-green-100 text-green-800 dark:bg-green-900/30 dark:text-green-400",
  indexing:
    "bg-blue-100 text-blue-800 dark:bg-blue-900/30 dark:text-blue-400",
  stale: "bg-yellow-100 text-yellow-800 dark:bg-yellow-900/30 dark:text-yellow-400",
  orphaned:
    "bg-gray-100 text-gray-600 dark:bg-gray-800/30 dark:text-gray-400",
  dead_letter:
    "bg-red-100 text-red-800 dark:bg-red-900/30 dark:text-red-400",
};

export function Repos() {
  const api = useAdminApi();
  const [repos, setRepos] = useState<Repo[]>([]);
  const [error, setError] = useState("");
  const [newUrl, setNewUrl] = useState("");
  const [newBranch, setNewBranch] = useState("");

  function load() {
    api
      .get<Repo[]>("/repos")
      .then(setRepos)
      .catch((e) => setError(e.message));
  }

  useEffect(() => {
    load();
  }, [api]);

  async function handleAdd(e: FormEvent) {
    e.preventDefault();
    if (!newUrl) return;
    try {
      await api.post("/repos", {
        url: newUrl,
        branch: newBranch || undefined,
      });
      setNewUrl("");
      setNewBranch("");
      load();
    } catch (err) {
      setError(err instanceof Error ? err.message : "Failed to add repo");
    }
  }

  async function handleReindex(id: string) {
    try {
      await api.post(`/repos/${encodeURIComponent(id)}/reindex`);
      load();
    } catch (err) {
      setError(err instanceof Error ? err.message : "Failed to trigger reindex");
    }
  }

  async function handleRemove(id: string) {
    if (!confirm(`Remove repo ${id}?`)) return;
    try {
      await api.del(`/repos/${encodeURIComponent(id)}`);
      load();
    } catch (err) {
      setError(err instanceof Error ? err.message : "Failed to remove repo");
    }
  }

  return (
    <div className="space-y-6">
      <h2 className="text-lg font-semibold text-[var(--color-text)]">
        Repositories
      </h2>

      {error && (
        <div className="rounded-lg border border-red-300 bg-red-50 p-3 text-sm text-red-700 dark:border-red-800 dark:bg-red-900/20 dark:text-red-400">
          {error}
        </div>
      )}

      {/* Add repo form */}
      <form onSubmit={handleAdd} className="flex gap-2">
        <input
          type="text"
          value={newUrl}
          onChange={(e) => setNewUrl(e.target.value)}
          placeholder="https://github.com/org/repo"
          className="flex-1 rounded-md border border-[var(--color-border)] bg-[var(--color-surface)] px-3 py-1.5 text-sm text-[var(--color-text)] outline-none focus:border-[var(--color-graph-selection)]"
        />
        <input
          type="text"
          value={newBranch}
          onChange={(e) => setNewBranch(e.target.value)}
          placeholder="branch (optional)"
          className="w-40 rounded-md border border-[var(--color-border)] bg-[var(--color-surface)] px-3 py-1.5 text-sm text-[var(--color-text)] outline-none focus:border-[var(--color-graph-selection)]"
        />
        <button
          type="submit"
          disabled={!newUrl}
          className="rounded-md bg-[var(--color-graph-selection)] px-4 py-1.5 text-sm font-medium text-white disabled:opacity-50"
        >
          Add Repo
        </button>
      </form>

      {/* Repos table */}
      <div className="overflow-x-auto rounded-lg border border-[var(--color-border)]">
        <table className="w-full text-sm">
          <thead>
            <tr className="border-b border-[var(--color-border)] bg-[var(--color-surface-alt)]">
              <th className="px-4 py-2 text-left font-medium text-[var(--color-text-muted)]">
                Repo
              </th>
              <th className="px-4 py-2 text-left font-medium text-[var(--color-text-muted)]">
                Status
              </th>
              <th className="px-4 py-2 text-left font-medium text-[var(--color-text-muted)]">
                SHA
              </th>
              <th className="px-4 py-2 text-left font-medium text-[var(--color-text-muted)]">
                Freshness
              </th>
              <th className="px-4 py-2 text-left font-medium text-[var(--color-text-muted)]">
                Next Poll
              </th>
              <th className="px-4 py-2 text-left font-medium text-[var(--color-text-muted)]">
                Symbols
              </th>
              <th className="px-4 py-2 text-right font-medium text-[var(--color-text-muted)]">
                Actions
              </th>
            </tr>
          </thead>
          <tbody>
            {repos.map((repo) => (
              <tr
                key={repo.id}
                className="border-b border-[var(--color-border)] last:border-0"
              >
                <td className="px-4 py-2 font-medium text-[var(--color-text)]">
                  {repo.id}
                </td>
                <td className="px-4 py-2">
                  <span
                    className={`inline-block rounded-full px-2 py-0.5 text-xs font-medium ${statusColors[repo.status] ?? "bg-gray-100 text-gray-600"}`}
                  >
                    {repo.status}
                  </span>
                </td>
                <td className="px-4 py-2 font-mono text-xs text-[var(--color-text-muted)]">
                  {repo.indexed_sha?.slice(0, 7) ?? "-"}
                </td>
                <td className="px-4 py-2 text-[var(--color-text-muted)]">
                  {repo.freshness ?? "-"}
                </td>
                <td className="px-4 py-2 text-[var(--color-text-muted)]">
                  {repo.next_poll ?? "-"}
                </td>
                <td className="px-4 py-2 text-[var(--color-text-muted)]">
                  {repo.symbol_count?.toLocaleString() ?? "-"}
                </td>
                <td className="px-4 py-2 text-right">
                  <button
                    onClick={() => handleReindex(repo.id)}
                    className="mr-2 rounded px-2 py-1 text-xs text-[var(--color-graph-selection)] hover:bg-[var(--color-surface-alt)]"
                  >
                    Re-index
                  </button>
                  <button
                    onClick={() => handleRemove(repo.id)}
                    className="rounded px-2 py-1 text-xs text-red-500 hover:bg-red-50 dark:hover:bg-red-900/20"
                  >
                    Remove
                  </button>
                </td>
              </tr>
            ))}
            {repos.length === 0 && (
              <tr>
                <td
                  colSpan={7}
                  className="px-4 py-8 text-center text-[var(--color-text-muted)]"
                >
                  No repositories configured.
                </td>
              </tr>
            )}
          </tbody>
        </table>
      </div>
    </div>
  );
}
