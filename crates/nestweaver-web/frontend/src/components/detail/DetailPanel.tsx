import { isFileSelection, isNoteSelection, isSymbolKind } from "../../api/kinds";
import { useStore } from "../../stores";
import { GlassPanel } from "../panels/GlassPanel";
import { DiffDetail } from "./DiffDetail";
import { FileDetail } from "./FileDetail";
import { FlowDetail } from "./FlowDetail";
import { GapDetail } from "./GapDetail";
import { LlmResultDetail } from "../llm/LlmResultDetail";
import { NoteDetail } from "./NoteDetail";
import { PathDetail } from "./PathDetail";
import { SymbolDetail } from "./SymbolDetail";
import { NodeActionBar } from "../actions/NodeActionBar";
import { LensSummaryPanel } from "../workspace/LensSummaryPanel";
import { useSymbolQuery } from "../../hooks/useSymbolQuery";

function uidDisplayName(uid: string): string {
  // repo:<instance>:<name> / svc:repo:<instance>:<name>:<hash>
  const repo = /repo:[^:]+:([^:]+)/.exec(uid);
  if (repo) return repo[1];
  return uid.split(":").pop() || uid;
}

/** Repos, services and other non-symbol nodes: name first, uid as metadata (nw-572). */
function ContainerDetail({ uid, kind }: { uid: string; kind: string | null }) {
  const graphInstance = useStore((s) => s.graphInstance);
  const attr = (name: string) =>
    graphInstance?.hasNode(uid)
      ? (graphInstance.getNodeAttribute(uid, name) as string | undefined)
      : undefined;
  const name = attr("label") || uidDisplayName(uid);
  const nodeKind = attr("kind") ?? kind;
  const kindLabel =
    nodeKind === "repo" ? "Repository" : nodeKind === "service" ? "Service" : nodeKind ?? "Node";
  const reason = attr("reason");
  const location = attr("location");
  return (
    <div className="space-y-2 p-4">
      <p className="text-[10px] font-medium uppercase tracking-wide text-[var(--color-text-muted)]">
        {kindLabel}
      </p>
      <h2 className="break-words text-sm font-semibold text-[var(--color-text)]">{name}</h2>
      {reason && <p className="text-xs leading-5 text-[var(--color-text-muted)]">{reason}</p>}
      {location && (
        <p className="break-all text-[11px] text-[var(--color-text-muted)]">{location}</p>
      )}
      <p className="break-all font-mono text-[10px] text-[var(--color-text-muted)]" title="Node UID">
        {uid}
      </p>
    </div>
  );
}

export function DetailPanel() {
  const selectedNodeId = useStore((s) => s.selectedNodeId);
  const selectedNodeKind = useStore((s) => s.selectedNodeKind);
  const flowTraceActive = useStore((s) => s.flowTraceActive);
  const pathfindingActive = useStore((s) => s.pathfindingActive);
  const diffActive = useStore((s) => s.diffActive);
  const gapActive = useStore((s) => s.gapActive);
  const llmResult = useStore((s) => s.llmResult);
  const isSymbolSelection = Boolean(
    selectedNodeId && (selectedNodeId.startsWith("sym:") || isSymbolKind(selectedNodeKind)),
  );
  // Shared with SymbolDetail/Evidence/breadcrumb: one request per selection.
  const symbolQuery = useSymbolQuery(isSymbolSelection ? selectedNodeId : null);

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
          <LensSummaryPanel />
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

  const isSymbol = isSymbolSelection;
  const isNote = isNoteSelection(selectedNodeId, selectedNodeKind);
  const isFile = isFileSelection(selectedNodeId, selectedNodeKind);

  return (
    <GlassPanel data-testid="detail-panel" className="flex h-full flex-col border-l border-[var(--color-border)] bg-[var(--color-surface)]">
      <div className="border-b border-[var(--color-border)] p-2">
        <NodeActionBar
          node={{
            uid: selectedNodeId,
            kind: selectedNodeKind,
            label: symbolQuery.detail?.symbol.name,
            missing: symbolQuery.status === "missing",
          }}
          compact
        />
      </div>
      <div className="border-b border-[var(--color-border)] p-2">
        <LensSummaryPanel compact />
      </div>
      <div className="min-h-0 flex-1 overflow-hidden">
        {llmResult && <LlmResultDetail />}
        {diffActive && <DiffDetail />}
        {gapActive && <GapDetail />}
        {flowTraceActive && <FlowDetail />}
        {pathfindingActive && <PathDetail />}
        {isSymbol ? (
          <SymbolDetail key={selectedNodeId} uid={selectedNodeId} />
        ) : isNote ? (
          <NoteDetail key={selectedNodeId} uid={selectedNodeId} />
        ) : isFile ? (
          <FileDetail key={selectedNodeId} path={selectedNodeId} />
        ) : (
          <ContainerDetail uid={selectedNodeId} kind={selectedNodeKind} />
        )}
      </div>
    </GlassPanel>
  );
}
