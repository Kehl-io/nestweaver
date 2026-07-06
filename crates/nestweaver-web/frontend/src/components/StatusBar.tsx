import { useEffect, useState } from "react";
import { api } from "../api/client";
import { useStore } from "../stores";
import { useLiveUpdates } from "../sse/useLiveUpdates";
import { useWasmEngine } from "../hooks/useWasmEngine";
import type { BrainStatus, Repo } from "../api/types";
import { WorkspaceStatusChip } from "./workspace/WorkspaceStatusChip";

export function StatusBar() {
  useLiveUpdates();
  const wasm = useWasmEngine();

  const [status, setStatus] = useState<BrainStatus | null>(null);
  const [repos, setRepos] = useState<Repo[]>([]);
  const mode = useStore((s) => s.graphMode);
  const sseConnected = useStore((s) => s.sseConnected);
  const selectedWorkspace = useStore((s) => s.selectedWorkspace());
  const sceneMetadata = useStore((s) => s.sceneMetadata);

  useEffect(() => {
    api.brainStatus().then(setStatus).catch(() => {});
    api.repos().then(setRepos).catch(() => {});
  }, []);

  return (
    <footer data-testid="status-bar" className="flex h-6 shrink-0 items-center gap-3 overflow-hidden border-t border-[var(--color-border)] px-4 text-xs text-[var(--color-text-muted)]">
      <WorkspaceStatusChip
        workspace={selectedWorkspace}
        metadata={sceneMetadata}
      />
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
      {wasm.enabled && (
        <span
          className={
            wasm.ready
              ? "text-[var(--color-graph-selection)]"
              : "text-[var(--color-text-muted)]"
          }
          title={
            wasm.ready
              ? `WASM: ${wasm.nodeCount} nodes, ${wasm.edgeCount} edges`
              : "WASM: initializing"
          }
        >
          {wasm.ready ? `WASM: ready (${wasm.nodeCount}n)` : "WASM: loading…"}
        </span>
      )}
      <span className="ml-auto capitalize">{mode}</span>
    </footer>
  );
}
