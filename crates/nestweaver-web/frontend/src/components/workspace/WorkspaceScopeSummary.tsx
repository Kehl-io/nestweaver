import type { SceneMetadata, WorkspaceEntry } from "../../api/p1Types";
import { useStore } from "../../stores";
import { WorkspaceStatusChip } from "./WorkspaceStatusChip";

interface WorkspaceScopeSummaryProps {
  metadata?: SceneMetadata | null;
  compact?: boolean;
}

function countLabel(workspace: WorkspaceEntry | null): string {
  if (!workspace) return "No workspace counts";
  const counts = workspace.counts;
  const parts: string[] = [];
  if (counts.project_count > 0) parts.push(`${counts.project_count} project${counts.project_count === 1 ? "" : "s"}`);
  if (counts.repo_count > 0) parts.push(`${counts.repo_count} repo${counts.repo_count === 1 ? "" : "s"}`);
  if (counts.vault_count > 0) parts.push(`${counts.vault_count} vault${counts.vault_count === 1 ? "" : "s"}`);
  if (counts.symbol_count > 0) parts.push(`${counts.symbol_count} symbol${counts.symbol_count === 1 ? "" : "s"}`);
  if (counts.note_count > 0) parts.push(`${counts.note_count} note${counts.note_count === 1 ? "" : "s"}`);
  return parts.length > 0 ? parts.join(" · ") : "No indexed content";
}

function scopeLabel(workspace: WorkspaceEntry | null): string {
  if (!workspace) return "All content";
  if (workspace.type === "all") return "All content";
  if (workspace.type === "project") return "Project scope";
  if (workspace.type === "repo") return "Repo scope";
  if (workspace.type === "vault") return "Vault scope";
  return workspace.type;
}

export function WorkspaceScopeSummary({
  metadata,
  compact = false,
}: WorkspaceScopeSummaryProps) {
  const workspace = useStore((s) => s.selectedWorkspace());
  const sceneMetadata = useStore((s) => s.sceneMetadata);
  const activeMetadata = metadata ?? sceneMetadata ?? workspace?._meta ?? null;
  const unsupported = activeMetadata?.trust.unsupported ?? [];

  if (compact) {
    return (
      <div className="min-w-0 space-y-1 text-[11px] text-[var(--color-text-muted)]">
        <p className="line-clamp-2" title={`${scopeLabel(workspace)} · ${countLabel(workspace)}`}>
          {scopeLabel(workspace)} · {countLabel(workspace)}
        </p>
        <WorkspaceStatusChip
          workspace={workspace}
          metadata={activeMetadata}
          compact
        />
      </div>
    );
  }

  return (
    <div className="space-y-2 rounded border border-[var(--color-border)] bg-[var(--color-surface-alt)]/55 p-2 text-xs">
      <div className="flex min-w-0 items-center justify-between gap-2">
        <div className="min-w-0">
          <p className="truncate font-medium text-[var(--color-text)]">
            {workspace?.label ?? "All indexed content"}
          </p>
          <p className="mt-0.5 text-[11px] text-[var(--color-text-muted)]">
            {scopeLabel(workspace)} · {countLabel(workspace)}
          </p>
        </div>
        <WorkspaceStatusChip
          workspace={workspace}
          metadata={activeMetadata}
          compact
        />
      </div>
      {unsupported.length > 0 && (
        <p className="text-[11px] text-amber-500">
          Limited: {unsupported.join(", ")}
        </p>
      )}
    </div>
  );
}
