import { useEffect, useState, useCallback } from "react";
import { useAdminApi } from "../../hooks/useAdminApi";

interface QueueState {
  depth: number;
  by_priority?: Record<string, number>;
  running?: Array<{
    repo: string;
    started_at: string;
    duration_s: number;
  }>;
}

interface DrainStatus {
  drained: boolean;
  in_flight: number;
  queued: number;
}

const priorityColors: Record<string, string> = {
  webhook:
    "bg-purple-100 text-purple-800 dark:bg-purple-900/30 dark:text-purple-400",
  poll: "bg-blue-100 text-blue-800 dark:bg-blue-900/30 dark:text-blue-400",
  scheduled:
    "bg-gray-100 text-gray-600 dark:bg-gray-800/30 dark:text-gray-400",
};

export function Queue() {
  const api = useAdminApi();
  const [queue, setQueue] = useState<QueueState | null>(null);
  const [drain, setDrain] = useState<DrainStatus | null>(null);
  const [error, setError] = useState("");

  const load = useCallback(() => {
    api
      .get<QueueState>("/queue")
      .then(setQueue)
      .catch((e) => setError(e.message));
    api
      .get<DrainStatus>("/drain/status")
      .then(setDrain)
      .catch(() => {});
  }, [api]);

  useEffect(() => {
    load();
    const interval = setInterval(load, 5000);
    return () => clearInterval(interval);
  }, [load]);

  async function toggleDrain() {
    try {
      if (drain?.drained) {
        await api.post("/resume");
      } else {
        await api.post("/drain");
      }
      load();
    } catch (err) {
      setError(err instanceof Error ? err.message : "Failed to toggle drain");
    }
  }

  return (
    <div className="space-y-6">
      <div className="flex items-center justify-between">
        <h2 className="text-lg font-semibold text-[var(--color-text)]">
          Job Queue
        </h2>
        <div className="flex items-center gap-3">
          {drain && (
            <span
              className={`rounded-full px-2.5 py-0.5 text-xs font-medium ${
                drain.drained
                  ? "bg-yellow-100 text-yellow-800 dark:bg-yellow-900/30 dark:text-yellow-400"
                  : "bg-green-100 text-green-800 dark:bg-green-900/30 dark:text-green-400"
              }`}
            >
              {drain.drained ? "Drained" : "Running"}
            </span>
          )}
          <button
            onClick={toggleDrain}
            className={`rounded-md px-3 py-1.5 text-sm font-medium text-white ${
              drain?.drained
                ? "bg-green-600 hover:bg-green-700"
                : "bg-yellow-600 hover:bg-yellow-700"
            }`}
          >
            {drain?.drained ? "Resume" : "Drain"}
          </button>
        </div>
      </div>

      {error && (
        <div className="rounded-lg border border-red-300 bg-red-50 p-3 text-sm text-red-700 dark:border-red-800 dark:bg-red-900/20 dark:text-red-400">
          {error}
        </div>
      )}

      {queue && (
        <>
          {/* Summary cards */}
          <div className="grid grid-cols-3 gap-4">
            <div className="rounded-lg border border-[var(--color-border)] bg-[var(--color-surface-alt)] p-4">
              <div className="text-sm text-[var(--color-text-muted)]">
                Queue Depth
              </div>
              <div className="mt-1 text-2xl font-semibold text-[var(--color-text)]">
                {queue.depth}
              </div>
            </div>
            <div className="rounded-lg border border-[var(--color-border)] bg-[var(--color-surface-alt)] p-4">
              <div className="text-sm text-[var(--color-text-muted)]">
                Running
              </div>
              <div className="mt-1 text-2xl font-semibold text-[var(--color-text)]">
                {queue.running?.length ?? 0}
              </div>
            </div>
            <div className="rounded-lg border border-[var(--color-border)] bg-[var(--color-surface-alt)] p-4">
              <div className="text-sm text-[var(--color-text-muted)]">
                By Priority
              </div>
              <div className="mt-2 flex flex-wrap gap-1.5">
                {queue.by_priority &&
                  Object.entries(queue.by_priority).map(([k, v]) => (
                    <span
                      key={k}
                      className={`rounded-full px-2 py-0.5 text-xs font-medium ${priorityColors[k] ?? "bg-gray-100 text-gray-600"}`}
                    >
                      {k}: {v}
                    </span>
                  ))}
              </div>
            </div>
          </div>

          {/* Running jobs table */}
          {queue.running && queue.running.length > 0 && (
            <div className="overflow-x-auto rounded-lg border border-[var(--color-border)]">
              <table className="w-full text-sm">
                <thead>
                  <tr className="border-b border-[var(--color-border)] bg-[var(--color-surface-alt)]">
                    <th className="px-4 py-2 text-left font-medium text-[var(--color-text-muted)]">
                      Repo
                    </th>
                    <th className="px-4 py-2 text-left font-medium text-[var(--color-text-muted)]">
                      Started
                    </th>
                    <th className="px-4 py-2 text-left font-medium text-[var(--color-text-muted)]">
                      Duration
                    </th>
                  </tr>
                </thead>
                <tbody>
                  {queue.running.map((job, i) => (
                    <tr
                      key={i}
                      className="border-b border-[var(--color-border)] last:border-0"
                    >
                      <td className="px-4 py-2 font-medium text-[var(--color-text)]">
                        {job.repo}
                      </td>
                      <td className="px-4 py-2 text-[var(--color-text-muted)]">
                        {job.started_at}
                      </td>
                      <td className="px-4 py-2 text-[var(--color-text-muted)]">
                        {job.duration_s.toFixed(1)}s
                      </td>
                    </tr>
                  ))}
                </tbody>
              </table>
            </div>
          )}

          <p className="text-xs text-[var(--color-text-muted)]">
            Auto-refreshing every 5 seconds
          </p>
        </>
      )}

      {!queue && !error && (
        <div className="text-[var(--color-text-muted)]">Loading...</div>
      )}
    </div>
  );
}
