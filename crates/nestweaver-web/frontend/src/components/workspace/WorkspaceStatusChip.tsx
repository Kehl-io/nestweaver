import type { SceneMetadata, WorkspaceEntry } from "../../api/p1Types";
import { useStore } from "../../stores";

interface WorkspaceStatusChipProps {
  workspace?: WorkspaceEntry | null;
  metadata?: SceneMetadata | null;
  compact?: boolean;
}

function stateTone(metadata: SceneMetadata | null | undefined): string {
  const trust = metadata?.trust;
  if (!trust) return "border-[var(--color-border)] text-[var(--color-text-muted)]";
  if (trust.result === "error" || trust.result === "timed-out") {
    return "border-red-400/70 text-red-500";
  }
  if (trust.result === "unsupported" || trust.unsupported.length > 0) {
    return "border-amber-400/60 text-amber-500";
  }
  if (trust.result === "empty" || trust.result === "no-match") {
    return "border-zinc-400/60 text-[var(--color-text-muted)]";
  }
  if (trust.partial || trust.freshness === "partial" || trust.result === "partial") {
    return "border-sky-400/60 text-sky-500";
  }
  if (trust.freshness === "stale" || trust.result === "truncated") {
    return "border-amber-400/60 text-amber-500";
  }
  return "border-emerald-400/60 text-emerald-500";
}

function resultLabel(result: string | undefined): string | null {
  switch (result) {
    case "empty":
      return "empty";
    case "no-match":
      return "no match";
    case "unsupported":
      return "unsupported";
    case "timed-out":
      return "timed out";
    case "error":
      return "error";
    case "truncated":
      return "truncated";
    case "loading":
      return "loading";
    case "ambiguous":
      return "ambiguous";
    case "cancelled":
      return "cancelled";
    default:
      return null;
  }
}

function statusText(metadata: SceneMetadata | null | undefined): string {
  const trust = metadata?.trust;
  if (!trust) return "unknown";
  const parts = [trust.federation, trust.freshness].filter(Boolean);
  if (trust.partial && !parts.includes("partial")) parts.push("partial");
  const result = resultLabel(trust.result);
  if (result && !parts.includes(result)) parts.push(result);
  return parts.join(" · ");
}

export function WorkspaceStatusChip({
  workspace,
  metadata,
  compact = false,
}: WorkspaceStatusChipProps) {
  const selectedWorkspace = useStore((s) => s.selectedWorkspace());
  const sceneMetadata = useStore((s) => s.sceneMetadata);
  const activeWorkspace = workspace ?? selectedWorkspace;
  const activeMetadata = metadata ?? sceneMetadata ?? activeWorkspace?._meta ?? null;
  const label = activeWorkspace?.label ?? "All indexed content";
  const text = statusText(activeMetadata);

  return (
    <span
      title={activeMetadata?.trust.message ?? `${label}: ${text}`}
      className={`inline-flex min-w-0 max-w-full items-center gap-1 rounded border bg-[var(--color-surface-alt)]/70 px-2 py-0.5 font-medium ${stateTone(activeMetadata)}`}
    >
      {!compact && (
        <span className="truncate text-[var(--color-text)]">{label}</span>
      )}
      <span className={compact ? "truncate" : "shrink-0"}>{text}</span>
    </span>
  );
}
