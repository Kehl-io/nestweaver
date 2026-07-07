import { useMemo } from "react";
import { Copy, Download } from "lucide-react";
import type {
  AffectedTestsResult,
  ImpactLensStates,
} from "../../api/impactLens";
import type { BrainContextResult } from "../../api/types";
import { useStore } from "../../stores";

const JSON_CAPS = {
  graphNodes: 500,
  graphEdges: 1000,
  flowRows: 250,
  flowNodeUids: 250,
  pathResults: 50,
  gapItems: 100,
  relationshipRows: 100,
  backlinkRows: 100,
  diffSeeds: 100,
  diffConnected: 200,
  diffUnresolved: 100,
  affectedTestFiles: 100,
  changedSymbols: 100,
};

type GraphInstance = NonNullable<ReturnType<typeof useStore.getState>["graphInstance"]>;

function attrsToRecord(attrs: unknown): Record<string, unknown> {
  return attrs && typeof attrs === "object"
    ? { ...(attrs as Record<string, unknown>) }
    : {};
}

function graphAttribute<T>(graph: GraphInstance, name: string): T | null {
  const value = graph.getAttribute(name) as T | undefined;
  return value ?? null;
}

function impactAttributes(graph: GraphInstance | null) {
  if (!graph) return null;
  const attributes = {
    impact_target: graphAttribute<string | null>(graph, "impactTarget"),
    impact_states: graphAttribute<ImpactLensStates>(graph, "impactStates"),
    affected_tests: graphAttribute<AffectedTestsResult>(graph, "affectedTests"),
  };
  return Object.values(attributes).some((value) => value != null)
    ? attributes
    : null;
}

function buildGraphPayload() {
  const graph = useStore.getState().graphInstance;
  if (!graph) {
    return {
      nodes: [],
      edges: [],
      unavailable_reason: "No graph data is loaded for the current scene.",
      _meta: {
        node_count: 0,
        edge_count: 0,
        nodes_truncated: false,
        edges_truncated: false,
        node_limit: JSON_CAPS.graphNodes,
        edge_limit: JSON_CAPS.graphEdges,
        omitted_nodes: 0,
        omitted_edges: 0,
      },
    };
  }
  const nodes: Record<string, unknown>[] = [];
  let nodeCount = 0;
  graph.forEachNode((uid, attrs) => {
    if (nodes.length < JSON_CAPS.graphNodes) {
      nodes.push({ uid, ...attrsToRecord(attrs) });
    }
    nodeCount += 1;
  });
  const edges: Record<string, unknown>[] = [];
  let edgeCount = 0;
  graph.forEachEdge((edgeId, attrs, source, target) => {
    if (edges.length < JSON_CAPS.graphEdges) {
      edges.push({ id: edgeId, source, target, ...attrsToRecord(attrs) });
    }
    edgeCount += 1;
  });
  return {
    nodes,
    edges,
    attributes: impactAttributes(graph),
    _meta: {
      node_count: nodeCount,
      edge_count: edgeCount,
      nodes_truncated: nodeCount > nodes.length,
      edges_truncated: edgeCount > edges.length,
      node_limit: JSON_CAPS.graphNodes,
      edge_limit: JSON_CAPS.graphEdges,
      omitted_nodes: Math.max(0, nodeCount - nodes.length),
      omitted_edges: Math.max(0, edgeCount - edges.length),
    },
  };
}

function impactAnalysisPayload(graph: GraphInstance | null) {
  const attributes = impactAttributes(graph);
  const affectedTests = attributes?.affected_tests ?? null;
  const tier1 = capList(affectedTests?.tier_1 ?? [], JSON_CAPS.affectedTestFiles);
  const tier2 = capList(affectedTests?.tier_2 ?? [], JSON_CAPS.affectedTestFiles);
  const tier3 = capList(affectedTests?.tier_3 ?? [], JSON_CAPS.affectedTestFiles);
  const changedSymbols = capList(
    affectedTests?.changed_symbols ?? [],
    JSON_CAPS.changedSymbols,
  );

  return {
    active: Boolean(attributes),
    target_uid: attributes?.impact_target ?? null,
    states: attributes?.impact_states ?? null,
    affected_tests: affectedTests
      ? {
          changed_files: affectedTests.changed_files,
          changed_symbols: changedSymbols.items,
          tier_1: tier1.items,
          tier_2: tier2.items,
          tier_3: tier3.items,
          summary: affectedTests.summary,
          disclaimer: affectedTests.disclaimer,
          _meta: {
            changed_symbols: changedSymbols._meta,
            tier_1: tier1._meta,
            tier_2: tier2._meta,
            tier_3: tier3._meta,
          },
        }
      : null,
  };
}

function flowToCappedList(
  node: ReturnType<typeof useStore.getState>["flowTraceRoot"],
  limit: number,
) {
  if (!node) {
    return {
      items: [] as Record<string, unknown>[],
      _meta: {
        exported_count: 0,
        total_count: 0,
        total_count_exact: true,
        limit,
        truncated: false,
        omitted_count: 0,
      },
    };
  }
  const rows: Record<string, unknown>[] = [];
  const stack: Array<{
    current: NonNullable<typeof node>;
    parentUid: string | null;
  }> = [{ current: node, parentUid: null }];
  let truncated = false;

  while (stack.length > 0 && rows.length < limit) {
    const { current, parentUid } = stack.pop()!;
    rows.push({
      uid: current.uid,
      name: current.name,
      file_path: current.file_path,
      depth: current.depth,
      parent_uid: parentUid,
      child_count: current.children.length,
    });

    if (rows.length >= limit) {
      truncated = stack.length > 0 || current.children.length > 0;
      break;
    }

    for (let index = current.children.length - 1; index >= 0; index -= 1) {
      stack.push({ current: current.children[index], parentUid: current.uid });
    }
  }

  if (stack.length > 0) truncated = true;

  return {
    items: rows,
    _meta: {
      exported_count: rows.length,
      total_count: truncated ? null : rows.length,
      total_count_exact: !truncated,
      limit,
      truncated,
      omitted_count: truncated ? null : 0,
      truncation_reason: truncated
        ? "Trace traversal stopped at the JSON export limit."
        : null,
    },
  };
}

function capList<T>(items: T[], limit: number) {
  return {
    items: items.slice(0, limit),
    _meta: {
      total_count: items.length,
      limit,
      truncated: items.length > limit,
      omitted_count: Math.max(0, items.length - limit),
    },
  };
}

function capBrainContextResult(
  snapshot: BrainContextResult | null,
): (BrainContextResult & { _meta: Record<string, unknown> }) | null {
  if (!snapshot) return null;
  const seeds = capList(snapshot.seeds, JSON_CAPS.diffSeeds);
  const connected = capList(snapshot.connected, JSON_CAPS.diffConnected);
  const unresolved = capList(snapshot.unresolved_seeds, JSON_CAPS.diffUnresolved);
  return {
    seeds: seeds.items,
    connected: connected.items,
    unresolved_seeds: unresolved.items,
    _meta: {
      capped: true,
      seeds: seeds._meta,
      connected: connected._meta,
      unresolved_seeds: unresolved._meta,
    },
  };
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
  const relationshipResult = useStore((s) => s.relationshipResult);
  const backlinkResult = useStore((s) => s.backlinkResult);
  const diffState = useStore((s) => s.diffState);
  const diffActive = useStore((s) => s.diffActive);
  const notify = useStore((s) => s.notify);

  const payload = useMemo(() => {
    void graphVersion;
    const graphInstance = useStore.getState().graphInstance;
    const graph = buildGraphPayload();
    const impact = impactAnalysisPayload(graphInstance);
    const flowRows = flowToCappedList(flowTraceRoot, JSON_CAPS.flowRows);
    const cappedFlowNodeUids = capList(flowTraceNodeUids, JSON_CAPS.flowNodeUids);
    const cappedPathResults = capList(pathResults, JSON_CAPS.pathResults);
    const cappedGapItems = capList(gapItems, JSON_CAPS.gapItems);
    const cappedRelationshipRows = capList(
      relationshipResult?.rows ?? [],
      JSON_CAPS.relationshipRows,
    );
    const cappedBacklinkRows = capList(
      backlinkResult?.rows ?? [],
      JSON_CAPS.backlinkRows,
    );
    const cappedSeedsA = capList(diffState.seedsA, JSON_CAPS.diffSeeds);
    const cappedSeedsB = capList(diffState.seedsB, JSON_CAPS.diffSeeds);
    return {
      _meta: {
        ...(sceneMetadata ?? {
          workspace_id: activeWorkspaceId,
          trust: trustSummary,
          unavailable_reason:
            "No scene metadata is available; showing current client-side scene state.",
        }),
        client_caps: {
          graph: graph._meta,
          flow_rows: flowRows._meta,
          flow_trace_node_uids: cappedFlowNodeUids._meta,
          path_results: cappedPathResults._meta,
          gap_items: cappedGapItems._meta,
          relationship_rows: cappedRelationshipRows._meta,
          backlink_rows: cappedBacklinkRows._meta,
          diff_snapshots: {
            seeds_limit: JSON_CAPS.diffSeeds,
            connected_limit: JSON_CAPS.diffConnected,
            unresolved_limit: JSON_CAPS.diffUnresolved,
          },
        },
      },
      active_lens: activeLens,
      selected_node: {
        uid: selectedNodeId,
        kind: selectedNodeKind,
      },
      representation: representationMode,
      graph,
      analysis: {
        impact,
        flow_trace: {
          active: Boolean(flowTraceRoot),
          node_uids: cappedFlowNodeUids.items,
          rows: flowRows.items,
          _meta: {
            rows: flowRows._meta,
            node_uids: cappedFlowNodeUids._meta,
          },
        },
        paths: {
          status: pathStatus,
          error: pathError,
          results: cappedPathResults.items,
          _meta: cappedPathResults._meta,
        },
        gaps: {
          active: gapActive,
          items: cappedGapItems.items,
          _meta: cappedGapItems._meta,
        },
        relationships: {
          active: Boolean(relationshipResult),
          kind: relationshipResult?.kind ?? null,
          target_uid: relationshipResult?.targetUid ?? null,
          target_label: relationshipResult?.targetLabel ?? null,
          workspace_id: relationshipResult?.workspaceId ?? null,
          status: relationshipResult?.status ?? "idle",
          error: relationshipResult?.error ?? null,
          rows: cappedRelationshipRows.items,
          _meta: {
            ...cappedRelationshipRows._meta,
            semantics:
              "Rows are direct symbol-detail caller/callee results when active; generic graph rows are contextual only.",
          },
        },
        backlinks: {
          active: Boolean(backlinkResult),
          target_uid: backlinkResult?.targetUid ?? null,
          target_label: backlinkResult?.targetLabel ?? null,
          workspace_id: backlinkResult?.workspaceId ?? null,
          status: backlinkResult?.status ?? "idle",
          error: backlinkResult?.error ?? null,
          rows: cappedBacklinkRows.items,
          _meta: {
            ...cappedBacklinkRows._meta,
            semantics:
              "Rows are direct backlinks API results for the active note target.",
          },
        },
        diff: {
          active: diffActive,
          seeds_a: cappedSeedsA.items,
          seeds_b: cappedSeedsB.items,
          seeds_meta: {
            seeds_a: cappedSeedsA._meta,
            seeds_b: cappedSeedsB._meta,
          },
          snapshot_a: capBrainContextResult(diffState.snapshotA),
          snapshot_b: capBrainContextResult(diffState.snapshotB),
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
    relationshipResult,
    backlinkResult,
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
