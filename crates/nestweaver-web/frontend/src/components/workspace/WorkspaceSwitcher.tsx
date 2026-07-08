import { useEffect, useRef } from "react";
import * as Select from "@radix-ui/react-select";
import {
  ChevronDown,
  Database,
  FolderKanban,
  FolderGit2,
  Layers,
  NotebookTabs,
  RefreshCw,
} from "lucide-react";
import type { WorkspaceEntry, WorkspaceType } from "../../api/p1Types";
import { useStore } from "../../stores";

function typeIcon(type: WorkspaceType) {
  switch (type) {
    case "project":
      return FolderKanban;
    case "repo":
      return FolderGit2;
    case "vault":
      return NotebookTabs;
    case "all":
      return Layers;
  }
}

function typeLabel(type: WorkspaceType): string {
  switch (type) {
    case "all":
      return "All";
    case "project":
      return "Project";
    case "repo":
      return "Repo";
    case "vault":
      return "Vault";
  }
}

function countHint(workspace: WorkspaceEntry): string {
  if (workspace.type === "project") {
    return `${workspace.counts.symbol_count + workspace.counts.note_count} members`;
  }
  if (workspace.type === "repo") {
    return `${workspace.counts.symbol_count} symbols`;
  }
  if (workspace.type === "vault") {
    return `${workspace.counts.note_count} notes`;
  }
  const total = workspace.counts.symbol_count + workspace.counts.note_count;
  return `${total} items`;
}

export function WorkspaceSwitcher() {
  const autoLoadAttemptedRef = useRef(false);
  const workspaces = useStore((s) => s.workspaces);
  const activeWorkspaceId = useStore((s) => s.activeWorkspaceId);
  const selectedWorkspace = useStore((s) => s.selectedWorkspace());
  const loading = useStore((s) => s.workspacesLoading);
  const error = useStore((s) => s.workspacesError);
  const loadWorkspaces = useStore((s) => s.loadWorkspaces);
  const setActiveWorkspaceId = useStore((s) => s.setActiveWorkspaceId);
  const setSceneMetadata = useStore((s) => s.setSceneMetadata);
  const clearGraphData = useStore((s) => s.clearGraphData);
  const clearWorkspaceError = useStore((s) => s.clearWorkspaceError);
  const notify = useStore((s) => s.notify);

  useEffect(() => {
    if (workspaces.length === 0 && !loading && !autoLoadAttemptedRef.current) {
      autoLoadAttemptedRef.current = true;
      void loadWorkspaces();
    }
  }, [loadWorkspaces, loading, workspaces.length]);

  useEffect(() => {
    if (error) {
      notify({
        kind: "error",
        title: "Workspaces failed",
        message: error,
      });
    }
  }, [error, notify]);

  const triggerLabel = selectedWorkspace?.label ?? "All indexed content";
  const TriggerIcon = selectedWorkspace ? typeIcon(selectedWorkspace.type) : Database;

  function handleRetry() {
    clearWorkspaceError();
    void loadWorkspaces();
  }

  function handleWorkspaceChange(id: string) {
    const nextWorkspace = workspaces.find((workspace) => workspace.id === id);
    setActiveWorkspaceId(id);
    setSceneMetadata(nextWorkspace?._meta ?? null);
    clearGraphData();
  }

  return (
    <div className="min-w-0 shrink-0">
      <Select.Root
        value={activeWorkspaceId}
        onValueChange={handleWorkspaceChange}
        disabled={loading && workspaces.length === 0}
      >
        <Select.Trigger
          aria-label="Workspace"
          title={`Workspace: ${triggerLabel}`}
          className="inline-flex h-8 max-w-[11rem] items-center justify-between gap-2 rounded border border-[var(--color-border)] bg-[var(--color-surface-alt)] px-2 text-xs text-[var(--color-text)] outline-none transition-colors hover:bg-[var(--color-surface)] focus-visible:ring-2 focus-visible:ring-[var(--color-graph-selection)] focus-visible:ring-offset-1 focus-visible:ring-offset-[var(--color-surface)] sm:max-w-[15rem]"
        >
          <TriggerIcon className="h-3.5 w-3.5 shrink-0 text-[var(--color-text-muted)]" />
          <Select.Value>
            <span className="truncate">{loading ? "Loading..." : triggerLabel}</span>
          </Select.Value>
          <Select.Icon asChild>
            <ChevronDown className="h-3.5 w-3.5 shrink-0 text-[var(--color-text-muted)]" />
          </Select.Icon>
        </Select.Trigger>
        <Select.Portal>
          <Select.Content
            position="popper"
            sideOffset={4}
            className="z-[100] max-h-[24rem] min-w-[18rem] overflow-hidden rounded-md border border-[var(--color-border)] bg-[var(--color-surface)] py-1 text-xs text-[var(--color-text)] shadow-xl"
          >
            <Select.Viewport>
              {workspaces.length === 0 && (
                <div className="space-y-2 px-3 py-2 text-[var(--color-text-muted)]">
                  <p>{loading ? "Loading workspaces..." : error ? "Workspaces failed to load" : "No workspaces found"}</p>
                  {error && (
                    <button
                      type="button"
                      onClick={handleRetry}
                      className="inline-flex h-7 items-center gap-1.5 rounded border border-[var(--color-border)] bg-[var(--color-surface-alt)] px-2 text-[11px] font-medium text-[var(--color-text)] outline-none hover:bg-[var(--color-surface)] focus-visible:ring-2 focus-visible:ring-[var(--color-graph-selection)]"
                    >
                      <RefreshCw className="h-3 w-3" />
                      Retry
                    </button>
                  )}
                </div>
              )}
              {workspaces.map((workspace) => {
                const Icon = typeIcon(workspace.type);
                return (
                  <Select.Item
                    key={workspace.id}
                    value={workspace.id}
                    className="relative cursor-default select-none px-3 py-2 outline-none data-[highlighted]:bg-[var(--color-surface-alt)] data-[highlighted]:text-[var(--color-text)] data-[state=checked]:text-[var(--color-graph-selection)]"
                  >
                    <Select.ItemText>
                      <span className="flex min-w-0 items-start gap-2">
                        <Icon className="mt-0.5 h-3.5 w-3.5 shrink-0 text-[var(--color-text-muted)]" />
                        <span className="min-w-0 flex-1">
                          <span className="block truncate font-medium">
                            {workspace.label}
                          </span>
                          <span className="mt-0.5 flex items-center gap-2 text-[11px] text-[var(--color-text-muted)]">
                            <span>{typeLabel(workspace.type)}</span>
                            <span>{countHint(workspace)}</span>
                          </span>
                        </span>
                      </span>
                    </Select.ItemText>
                  </Select.Item>
                );
              })}
            </Select.Viewport>
          </Select.Content>
        </Select.Portal>
      </Select.Root>
      <div className="sr-only" aria-live="polite">
        Workspace: {triggerLabel}
      </div>
    </div>
  );
}
