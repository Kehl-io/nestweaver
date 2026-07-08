import type { SceneMetadata, TrustSummary } from "../../api/p1Types";

interface TrustBadgeProps {
  metadata?: SceneMetadata | null;
  summary?: TrustSummary | null;
  state?: "loading" | "error" | "ready" | "empty";
}

function stateLabel(state?: TrustBadgeProps["state"]): string {
  switch (state) {
    case "loading":
      return "Loading";
    case "error":
      return "Error";
    case "empty":
      return "Limited";
    case "ready":
    default:
      return "Ready";
  }
}

function toneClass(state?: TrustBadgeProps["state"], partial?: boolean): string {
  if (state === "error") {
    return "border-red-500/40 bg-red-500/10 text-red-300";
  }
  if (state === "loading") {
    return "border-[var(--color-border)] bg-[var(--color-surface-alt)] text-[var(--color-text-muted)]";
  }
  if (partial || state === "empty") {
    return "border-amber-500/40 bg-amber-500/10 text-amber-200";
  }
  return "border-emerald-500/40 bg-emerald-500/10 text-emerald-200";
}

export function TrustBadge({ metadata, summary, state = "ready" }: TrustBadgeProps) {
  const trust = metadata?.trust;
  const dataScope = trust?.data_scope ?? summary?.dataScope ?? "local-only";
  const freshness = trust?.freshness ?? summary?.freshness ?? "unknown";
  const result = trust?.result ?? summary?.result ?? stateLabel(state).toLowerCase();
  const partial = Boolean(trust?.partial ?? summary?.partial);
  const message =
    trust?.message ??
    summary?.message ??
    (state === "loading"
      ? "Preview detail is loading."
      : "Local graph metadata is shown where available.");

  return (
    <div
      className={`inline-flex max-w-full items-center gap-1.5 rounded border px-2 py-1 text-[10px] font-medium ${toneClass(state, partial)}`}
      title={message}
    >
      <span className="truncate">{stateLabel(state)}</span>
      <span aria-hidden="true">/</span>
      <span className="truncate">{String(dataScope)}</span>
      <span aria-hidden="true">/</span>
      <span className="truncate">{String(freshness)}</span>
      {result && (
        <>
          <span aria-hidden="true">/</span>
          <span className="truncate">{String(result)}</span>
        </>
      )}
    </div>
  );
}
