import { isSymbolKind } from "../../api/kinds";
import { useStore } from "../../stores";
import { GlassPanel } from "../panels/GlassPanel";
import { DiffDetail } from "./DiffDetail";
import { FlowDetail } from "./FlowDetail";
import { GapDetail } from "./GapDetail";
import { LlmResultDetail } from "../llm/LlmResultDetail";
import { NoteDetail } from "./NoteDetail";
import { PathDetail } from "./PathDetail";
import { SymbolDetail } from "./SymbolDetail";
import { NodeActionBar } from "../actions/NodeActionBar";

export function DetailPanel() {
  const selectedNodeId = useStore((s) => s.selectedNodeId);
  const selectedNodeKind = useStore((s) => s.selectedNodeKind);
  const flowTraceActive = useStore((s) => s.flowTraceActive);
  const pathfindingActive = useStore((s) => s.pathfindingActive);
  const diffActive = useStore((s) => s.diffActive);
  const gapActive = useStore((s) => s.gapActive);
  const llmResult = useStore((s) => s.llmResult);

  if (!selectedNodeId) {
    return (
      <GlassPanel data-testid="detail-panel" className="flex h-full flex-col border-l border-[var(--color-border)] bg-[var(--color-surface)] p-4 text-sm text-[var(--color-text-muted)]">
        <div className="border-b border-[var(--color-border)] pb-3">
          <p className="text-xs font-medium uppercase tracking-wide text-[var(--color-text-muted)]">
            Details
          </p>
          <h2 className="mt-1 text-base font-semibold text-[var(--color-text)]">
            Ready when you select a node
          </h2>
          <p className="mt-1 text-xs leading-5">
            Pick a node to open source, trace impact, find paths, or ask a question.
          </p>
        </div>
        <div className="mt-4 space-y-3 text-xs">
          <div className="rounded border border-[var(--color-border)] bg-[var(--color-surface-alt)] px-3 py-2">
            <p className="font-medium text-[var(--color-text)]">Fast starts</p>
            <p className="mt-1 leading-5">
              Use Start Here to explore the highest-signal symbol, then follow actions here.
            </p>
          </div>
          <p>
            <kbd className="rounded border border-[var(--color-border)] bg-[var(--color-surface-alt)] px-1.5 py-0.5 font-mono text-[10px]">
              /
            </kbd>{" "}
            Search symbols &amp; notes
          </p>
          <p>
            <kbd className="rounded border border-[var(--color-border)] bg-[var(--color-surface-alt)] px-1.5 py-0.5 font-mono text-[10px]">
              Esc
            </kbd>{" "}
            Close search
          </p>
        </div>
      </GlassPanel>
    );
  }

  const isSymbol =
    selectedNodeId.startsWith("sym:") ||
    isSymbolKind(selectedNodeKind);
  const isNote =
    selectedNodeId.startsWith("note:") ||
    selectedNodeKind === "note" ||
    selectedNodeKind === "Note";

  return (
    <GlassPanel data-testid="detail-panel" className="flex h-full flex-col border-l border-[var(--color-border)] bg-[var(--color-surface)]">
      <div className="border-b border-[var(--color-border)] p-2">
        <NodeActionBar
          node={{ uid: selectedNodeId, kind: selectedNodeKind }}
          compact
        />
      </div>
      <div className="min-h-0 flex-1 overflow-hidden">
        {llmResult && <LlmResultDetail />}
        {diffActive && <DiffDetail />}
        {gapActive && <GapDetail />}
        {flowTraceActive && <FlowDetail />}
        {pathfindingActive && <PathDetail />}
        {isSymbol ? (
          <SymbolDetail uid={selectedNodeId} />
        ) : isNote ? (
          <NoteDetail uid={selectedNodeId} />
        ) : (
          <div className="p-4">
            <h2 className="mb-2 text-sm font-semibold">Selected</h2>
            <p className="break-all text-sm text-[var(--color-text-muted)]">
              {selectedNodeId}
            </p>
          </div>
        )}
      </div>
    </GlassPanel>
  );
}
