import { useMemo } from "react";
import { Copy, Download } from "lucide-react";
import { useStore } from "../../stores";

function attrsToRecord(attrs: unknown): Record<string, unknown> {
  return attrs && typeof attrs === "object"
    ? { ...(attrs as Record<string, unknown>) }
    : {};
}

function buildGraphPayload() {
  const graph = useStore.getState().graphInstance;
  if (!graph) {
    return {
      nodes: [],
      edges: [],
      unavailable_reason: "No graph data is loaded for the current scene.",
    };
  }
  const nodes: Record<string, unknown>[] = [];
  graph.forEachNode((uid, attrs) => {
    nodes.push({ uid, ...attrsToRecord(attrs) });
  });
  const edges: Record<string, unknown>[] = [];
  graph.forEachEdge((edgeId, attrs, source, target) => {
    edges.push({ id: edgeId, source, target, ...attrsToRecord(attrs) });
  });
  return { nodes, edges };
}

function flowToList(node: ReturnType<typeof useStore.getState>["flowTraceRoot"]) {
  if (!node) return [];
  const rows: Record<string, unknown>[] = [];
  const visit = (current: NonNullable<typeof node>, parentUid: string | null) => {
    rows.push({
      uid: current.uid,
      name: current.name,
      file_path: current.file_path,
      depth: current.depth,
      parent_uid: parentUid,
      child_count: current.children.length,
    });
    current.children.forEach((child) => visit(child, current.uid));
  };
  visit(node, null);
  return rows;
}

export function JsonResultView() {
  const graphVersion = useStore((s) => s.graphVersion);
  const activeLens = useStore((s) => s.activeLens);
  const sceneMetadata = useStore((s) => s.sceneMetadata);
  const trustSummary = useStore((s) => s.trustSummary);
  const selectedNodeId = useStore((s) => s.selectedNodeId);
  const selectedNodeKind = useStore((s) => s.selectedNodeKind);
  const representationMode = useStore((s) => s.representationMode);
  const activeWorkspaceId = useStore((s) => s.activeWorkspaceId);
  const flowTraceRoot = useStore((s) => s.flowTraceRoot);
  const flowTraceNodeUids = useStore((s) => s.flowTraceNodeUids);
  const pathResults = useStore((s) => s.pathResults);
  const pathStatus = useStore((s) => s.pathStatus);
  const pathError = useStore((s) => s.pathError);
  const gapItems = useStore((s) => s.gapItems);
  const gapActive = useStore((s) => s.gapActive);
  const diffState = useStore((s) => s.diffState);
  const diffActive = useStore((s) => s.diffActive);
  const notify = useStore((s) => s.notify);

  const payload = useMemo(() => {
    void graphVersion;
    const graph = buildGraphPayload();
    return {
      _meta: sceneMetadata ?? {
        workspace_id: activeWorkspaceId,
        trust: trustSummary,
        unavailable_reason:
          "No scene metadata is available; showing current client-side scene state.",
      },
      active_lens: activeLens,
      selected_node: {
        uid: selectedNodeId,
        kind: selectedNodeKind,
      },
      representation: representationMode,
      graph,
      analysis: {
        flow_trace: {
          active: Boolean(flowTraceRoot),
          node_uids: flowTraceNodeUids,
          rows: flowToList(flowTraceRoot),
        },
        paths: {
          status: pathStatus,
          error: pathError,
          results: pathResults,
        },
        gaps: {
          active: gapActive,
          items: gapItems,
        },
        diff: {
          active: diffActive,
          seeds_a: diffState.seedsA,
          seeds_b: diffState.seedsB,
          snapshot_a: diffState.snapshotA,
          snapshot_b: diffState.snapshotB,
        },
      },
      unsupported_or_limited: trustSummary?.unsupported ?? [],
    };
  }, [
    activeLens,
    activeWorkspaceId,
    diffActive,
    diffState,
    flowTraceNodeUids,
    flowTraceRoot,
    gapActive,
    gapItems,
    graphVersion,
    pathError,
    pathResults,
    pathStatus,
    representationMode,
    sceneMetadata,
    selectedNodeId,
    selectedNodeKind,
    trustSummary,
  ]);

  const json = useMemo(() => JSON.stringify(payload, null, 2), [payload]);

  async function copyJson() {
    if (!navigator.clipboard) {
      notify({
        kind: "warning",
        title: "Copy unavailable",
        message: "Clipboard access is not available in this browser context.",
      });
      return;
    }
    await navigator.clipboard.writeText(json);
    notify({
      kind: "success",
      title: "JSON copied",
      message: "Current scene JSON is on the clipboard.",
    });
  }

  function downloadJson() {
    const blob = new Blob([json], { type: "application/json" });
    const url = URL.createObjectURL(blob);
    const link = document.createElement("a");
    link.href = url;
    link.download = "nestweaver-scene.json";
    link.click();
    URL.revokeObjectURL(url);
  }

  return (
    <section
      role="region"
      aria-label="JSON result"
      className="flex h-full flex-col bg-[var(--color-surface)]"
    >
      <div className="flex shrink-0 items-center justify-between gap-2 border-b border-[var(--color-border)] px-3 py-2">
        <div className="min-w-0">
          <h2 className="truncate text-sm font-semibold text-[var(--color-text)]">
            Raw Scene JSON
          </h2>
          <p className="text-[11px] text-[var(--color-text-muted)]">
            Includes graph, analysis state, and `_meta` trust/provenance when available.
          </p>
        </div>
        <div className="flex shrink-0 gap-1.5">
          <button
            type="button"
            onClick={copyJson}
            className="inline-flex h-8 items-center gap-1.5 rounded border border-[var(--color-border)] px-2 text-xs text-[var(--color-text-muted)] hover:bg-[var(--color-surface-alt)] hover:text-[var(--color-text)]"
          >
            <Copy className="h-3.5 w-3.5" />
            Copy
          </button>
          <button
            type="button"
            onClick={downloadJson}
            className="inline-flex h-8 items-center gap-1.5 rounded border border-[var(--color-border)] px-2 text-xs text-[var(--color-text-muted)] hover:bg-[var(--color-surface-alt)] hover:text-[var(--color-text)]"
          >
            <Download className="h-3.5 w-3.5" />
            Save
          </button>
        </div>
      </div>
      <pre className="min-h-0 flex-1 overflow-auto p-3 font-mono text-[11px] leading-5 text-[var(--color-text)]">
        <code>{json}</code>
      </pre>
    </section>
  );
}
