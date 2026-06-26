import { useEffect, useState } from "react";
import { useAdminApi } from "../../hooks/useAdminApi";

interface ServerStatus {
  instance_id?: string;
  uptime_seconds?: number;
  db_size_bytes?: number;
  repos?: { total: number; indexed: number; stale: number; dead_letter: number };
  symbols?: { total: number };
  queue?: { pending: number; running: number; dead_letter: number };
  connections?: { grpc: number; mcp: number };
  workers?: { count: number };
}

function formatUptime(seconds?: number): string {
  if (!seconds) return "unknown";
  const d = Math.floor(seconds / 86400);
  const h = Math.floor((seconds % 86400) / 3600);
  const m = Math.floor((seconds % 3600) / 60);
  if (d > 0) return `${d}d ${h}h ${m}m`;
  if (h > 0) return `${h}h ${m}m`;
  return `${m}m`;
}

function formatBytes(bytes?: number): string {
  if (!bytes) return "unknown";
  if (bytes > 1e9) return `${(bytes / 1e9).toFixed(1)} GB`;
  if (bytes > 1e6) return `${(bytes / 1e6).toFixed(1)} MB`;
  if (bytes > 1e3) return `${(bytes / 1e3).toFixed(1)} KB`;
  return `${bytes} B`;
}

function StatCard({
  label,
  value,
  sub,
}: {
  label: string;
  value: string | number;
  sub?: string;
}) {
  return (
    <div className="rounded-lg border border-[var(--color-border)] bg-[var(--color-surface-alt)] p-4">
      <div className="text-sm text-[var(--color-text-muted)]">{label}</div>
      <div className="mt-1 text-2xl font-semibold text-[var(--color-text)]">
        {value}
      </div>
      {sub && (
        <div className="mt-0.5 text-xs text-[var(--color-text-muted)]">
          {sub}
        </div>
      )}
    </div>
  );
}

export function Overview() {
  const api = useAdminApi();
  const [status, setStatus] = useState<ServerStatus | null>(null);
  const [error, setError] = useState("");

  useEffect(() => {
    api
      .get<ServerStatus>("/status")
      .then(setStatus)
      .catch((e) => setError(e.message));
  }, [api]);

  if (error) {
    return (
      <div className="rounded-lg border border-red-300 bg-red-50 p-4 text-red-700 dark:border-red-800 dark:bg-red-900/20 dark:text-red-400">
        Failed to load status: {error}
      </div>
    );
  }

  if (!status) {
    return (
      <div className="text-[var(--color-text-muted)]">Loading...</div>
    );
  }

  const healthPct =
    status.repos && status.repos.total > 0
      ? Math.round((status.repos.indexed / status.repos.total) * 100)
      : 100;

  return (
    <div className="space-y-6">
      <h2 className="text-lg font-semibold text-[var(--color-text)]">
        Server Overview
        {status.instance_id && (
          <span className="ml-2 text-sm font-normal text-[var(--color-text-muted)]">
            ({status.instance_id})
          </span>
        )}
      </h2>

      <div className="grid grid-cols-2 gap-4 sm:grid-cols-3 lg:grid-cols-4">
        <StatCard label="Uptime" value={formatUptime(status.uptime_seconds)} />
        <StatCard
          label="Database Size"
          value={formatBytes(status.db_size_bytes)}
        />
        <StatCard
          label="Symbols"
          value={status.symbols?.total?.toLocaleString() ?? "0"}
        />
        <StatCard
          label="Repos"
          value={status.repos?.total ?? 0}
          sub={`${status.repos?.indexed ?? 0} indexed, ${status.repos?.stale ?? 0} stale`}
        />
      </div>

      {/* Index Health Bar */}
      <div>
        <div className="mb-1 flex justify-between text-sm text-[var(--color-text-muted)]">
          <span>Index Health</span>
          <span>{healthPct}%</span>
        </div>
        <div className="h-2 overflow-hidden rounded-full bg-[var(--color-border)]">
          <div
            className="h-full rounded-full bg-[var(--color-graph-selection)] transition-all"
            style={{ width: `${healthPct}%` }}
          />
        </div>
      </div>

      <div className="grid grid-cols-2 gap-4 sm:grid-cols-3">
        <StatCard
          label="Queue"
          value={status.queue?.pending ?? 0}
          sub={`${status.queue?.running ?? 0} running, ${status.queue?.dead_letter ?? 0} dead`}
        />
        <StatCard
          label="Connections"
          value={
            (status.connections?.grpc ?? 0) + (status.connections?.mcp ?? 0)
          }
          sub={`${status.connections?.grpc ?? 0} gRPC, ${status.connections?.mcp ?? 0} MCP`}
        />
        <StatCard
          label="Workers"
          value={status.workers?.count ?? 0}
        />
      </div>
    </div>
  );
}
