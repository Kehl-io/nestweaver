import { useEffect, useState } from "react";
import { useAdminApi } from "../../hooks/useAdminApi";

interface ServerStatus {
  instance_id?: string;
  workers?: { count: number };
  polling?: { min_interval_s: number; max_interval_s: number };
  webhook?: { endpoint: string; path: string };
  auth?: { mode: string };
  embedding?: { enabled: boolean; model?: string };
  backup?: {
    enabled: boolean;
    interval_s?: number;
    destination?: string;
    retention_days?: number;
  };
  listeners?: {
    grpc?: string;
    mcp?: string;
    web?: string;
    webhook?: string;
  };
}

function ConfigSection({
  title,
  children,
}: {
  title: string;
  children: React.ReactNode;
}) {
  return (
    <div className="rounded-lg border border-[var(--color-border)] bg-[var(--color-surface-alt)]">
      <div className="border-b border-[var(--color-border)] px-4 py-2">
        <h3 className="text-sm font-medium text-[var(--color-text)]">
          {title}
        </h3>
      </div>
      <div className="divide-y divide-[var(--color-border)]">{children}</div>
    </div>
  );
}

function ConfigRow({ label, value }: { label: string; value: string }) {
  return (
    <div className="flex items-center justify-between px-4 py-2.5">
      <span className="text-sm text-[var(--color-text-muted)]">{label}</span>
      <span className="font-mono text-sm text-[var(--color-text)]">
        {value}
      </span>
    </div>
  );
}

export function Settings() {
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
        Failed to load settings: {error}
      </div>
    );
  }

  if (!status) {
    return (
      <div className="text-[var(--color-text-muted)]">Loading...</div>
    );
  }

  return (
    <div className="space-y-6">
      <h2 className="text-lg font-semibold text-[var(--color-text)]">
        Settings
      </h2>

      <div className="grid gap-4 lg:grid-cols-2">
        <ConfigSection title="Server">
          <ConfigRow
            label="Instance ID"
            value={status.instance_id ?? "default"}
          />
          <ConfigRow
            label="Workers"
            value={String(status.workers?.count ?? "auto")}
          />
          <ConfigRow
            label="Auth Mode"
            value={status.auth?.mode ?? "bearer"}
          />
        </ConfigSection>

        <ConfigSection title="Listeners">
          <ConfigRow
            label="gRPC"
            value={status.listeners?.grpc ?? "-"}
          />
          <ConfigRow
            label="MCP HTTP"
            value={status.listeners?.mcp ?? "-"}
          />
          <ConfigRow
            label="Web UI"
            value={status.listeners?.web ?? "-"}
          />
          <ConfigRow
            label="Webhook"
            value={status.listeners?.webhook ?? "-"}
          />
        </ConfigSection>

        <ConfigSection title="Polling">
          <ConfigRow
            label="Min Interval"
            value={
              status.polling
                ? `${status.polling.min_interval_s}s`
                : "-"
            }
          />
          <ConfigRow
            label="Max Interval"
            value={
              status.polling
                ? `${status.polling.max_interval_s}s`
                : "-"
            }
          />
        </ConfigSection>

        <ConfigSection title="Embedding">
          <ConfigRow
            label="Enabled"
            value={status.embedding?.enabled ? "yes" : "no"}
          />
          <ConfigRow
            label="Model"
            value={status.embedding?.model ?? "-"}
          />
        </ConfigSection>

        {status.backup && (
          <ConfigSection title="Backup">
            <ConfigRow
              label="Enabled"
              value={status.backup.enabled ? "yes" : "no"}
            />
            <ConfigRow
              label="Interval"
              value={
                status.backup.interval_s
                  ? `${status.backup.interval_s}s`
                  : "-"
              }
            />
            <ConfigRow
              label="Destination"
              value={status.backup.destination ?? "-"}
            />
            <ConfigRow
              label="Retention"
              value={
                status.backup.retention_days
                  ? `${status.backup.retention_days} days`
                  : "-"
              }
            />
          </ConfigSection>
        )}
      </div>

      <p className="text-sm text-[var(--color-text-muted)]">
        Settings are read-only. Edit instance.toml and use the Reload button
        (or POST /admin/api/reload) to apply changes.
      </p>
    </div>
  );
}
