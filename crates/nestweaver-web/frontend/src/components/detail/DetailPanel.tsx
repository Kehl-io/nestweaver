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

const SYMBOL_KINDS = new Set(["Function", "Class", "Interface", "Method", "Module"]);

export function DetailPanel() {
  const selectedNodeId = useStore((s) => s.selectedNodeId);
  const selectedNodeKind = useStore((s) => s.selectedNodeKind);
  const flowTraceActive = useStore((s) => s.flowTraceActive);
  const pathfindingActive = useStore((s) => s.pathfindingActive);
  const pathResults = useStore((s) => s.pathResults);
  const diffActive = useStore((s) => s.diffActive);
  const gapActive = useStore((s) => s.gapActive);
  const llmResult = useStore((s) => s.llmResult);

  if (!selectedNodeId) {
    return (
      <GlassPanel data-testid="detail-panel" className="flex h-full flex-col items-center justify-center gap-3 border-l border-[var(--color-border)] bg-[var(--color-surface)] p-6 text-center text-sm text-[var(--color-text-muted)]">
        <p>Click a node in the graph to see its details here.</p>
        <div className="space-y-1 text-xs">
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

  const isSymbol = selectedNodeKind != null && SYMBOL_KINDS.has(selectedNodeKind);
  const isNote = selectedNodeId.startsWith("note:") || (!isSymbol && selectedNodeKind !== "file");

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
        {pathfindingActive && pathResults.length > 0 && <PathDetail />}
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
