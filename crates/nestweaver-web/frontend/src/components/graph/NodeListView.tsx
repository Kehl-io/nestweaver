import { useMemo, useState } from "react";
import { ArrowDownUp } from "lucide-react";
import type {
  AffectedTestsResult,
  ImpactLensStates,
} from "../../api/impactLens";
import { useStore } from "../../stores";
import type {
  BacklinkResultState,
  FlowNode,
  RelationshipResultState,
} from "../../stores/analysisSlice";
import { NodeActionBar } from "../actions/NodeActionBar";

type SortKey = "order" | "primary" | "kind" | "relationship" | "status" | "location";
type SortDirection = "asc" | "desc";

interface ResultRow {
  id: string;
  order: number;
  uid: string | null;
  primary: string;
  kind: string;
  relationship: string;
  status: string;
  location: string;
  metadata: string;
}

type GraphInstance = NonNullable<ReturnType<typeof useStore.getState>["graphInstance"]>;

function compareValues(a: string | number, b: string | number): number {
  if (typeof a === "number" && typeof b === "number") return a - b;
  return String(a).localeCompare(String(b));
}

function nodeName(graph: GraphInstance, uid: string): string {
  return (
    (graph.getNodeAttribute(uid, "label") as string | undefined) ||
    uid.split(":").pop() ||
    uid
  );
}

function nodeKind(graph: GraphInstance, uid: string): string {
  return (graph.getNodeAttribute(uid, "kind") as string | undefined) || "Unknown";
}

function nodeLocation(graph: GraphInstance, uid: string): string {
  return (
    (graph.getNodeAttribute(uid, "location") as string | undefined) ||
    (graph.getNodeAttribute(uid, "filePath") as string | undefined) ||
    (graph.getNodeAttribute(uid, "file_path") as string | undefined) ||
    ""
  );
}

function nodeRelevance(graph: GraphInstance, uid: string): number {
  return (
    (graph.getNodeAttribute(uid, "relevance") as number | undefined) ||
    (graph.getNodeAttribute(uid, "pagerank") as number | undefined) ||
    0
  );
}

function edgeSummary(
  graph: GraphInstance,
  sourceUid: string,
  targetUid: string,
): string | null {
  let summary: string | null = null;
  graph.forEachEdge((_edge, attrs, source, target) => {
    if (summary || source !== sourceUid || target !== targetUid) return;
    const type =
      (attrs.edgeType as string | undefined) ||
      (attrs.label as string | undefined) ||
      "related";
    const confidence =
      typeof attrs.confidence === "number"
        ? `, confidence ${attrs.confidence.toFixed(2)}`
        : "";
    summary = `${type}${confidence}`;
  });
  return summary;
}

function relationshipForNode(
  graph: GraphInstance,
  uid: string,
  targetUid: string | null | undefined,
  lensLabel: string,
): string {
  const lower = lensLabel.toLowerCase();
  if (targetUid && lower.startsWith("callers of")) {
    const summary = edgeSummary(graph, uid, targetUid);
    return summary
      ? `contextual incoming edge to target: ${summary}`
      : "contextual neighbor; direct caller relationship unverified";
  }
  if (targetUid && lower.startsWith("callees of")) {
    const summary = edgeSummary(graph, targetUid, uid);
    return summary
      ? `contextual outgoing edge from target: ${summary}`
      : "contextual neighbor; direct callee relationship unverified";
  }
  if (lower.startsWith("backlinks for")) {
    return "contextual rationale neighbor; backlink row unverified";
  }
  if (lower.includes("bridge")) {
    return "bridge hint; exact bridge score unavailable in P1 table state";
  }
  if (lower.includes("hub")) {
    return "hub hint ranked from current overview graph";
  }
  if (lower.includes("impact")) {
    const depth = graph.getNodeAttribute(uid, "depth") as number | undefined;
    const edgeType =
      (graph.getNodeAttribute(uid, "edgeType") as string | undefined) ||
      (graph.getNodeAttribute(uid, "edge_type") as string | undefined);
    return [
      depth != null ? `impact depth ${depth}` : "impact candidate",
      edgeType ? `via ${edgeType}` : null,
    ].filter(Boolean).join(", ");
  }
  if (lower.includes("search")) {
    return "search result in the current workspace scope";
  }
  return "current graph result";
}

function graphRows(
  graph: GraphInstance | null,
  activeLensLabel: string,
  targetUid: string | null | undefined,
  trustResult: string,
): ResultRow[] {
  if (!graph) {
    return [{
      id: "graph-unavailable",
      order: 0,
      uid: null,
      primary: "Graph rows unavailable",
      kind: "Unavailable",
      relationship: "No graph data is loaded for the current scene.",
      status: trustResult,
      location: "",
      metadata: "Try a supported scene or reload the current workspace.",
    }];
  }

  const rows: ResultRow[] = [];
  graph.forEachNode((uid) => {
    const relevance = nodeRelevance(graph, uid);
    rows.push({
      id: `node:${uid}`,
      order: rows.length,
      uid,
      primary: nodeName(graph, uid),
      kind: nodeKind(graph, uid),
      relationship: relationshipForNode(graph, uid, targetUid, activeLensLabel),
      status: trustResult,
      location: nodeLocation(graph, uid),
      metadata: [
        `degree ${graph.degree(uid)}`,
        relevance > 0 ? `rank ${relevance.toFixed(3)}` : null,
      ].filter(Boolean).join(", "),
    });
  });

  return rows;
}

function relationshipRows(result: RelationshipResultState | null): ResultRow[] | null {
  if (!result) return null;
  if (result.status === "error") {
    return [{
      id: "relationship-error",
      order: 0,
      uid: null,
      primary: `Direct ${result.kind} unavailable`,
      kind: "Relationship state",
      relationship: `Direct ${result.kind} for ${result.targetLabel} could not be loaded.`,
      status: "error",
      location: "",
      metadata: result.error ?? "The symbol detail API returned an error.",
    }];
  }
  if (result.status === "empty" || result.rows.length === 0) {
    return [{
      id: "relationship-empty",
      order: 0,
      uid: null,
      primary: `No direct ${result.kind}`,
      kind: "Relationship state",
      relationship: `The symbol detail API returned no direct ${result.kind} for ${result.targetLabel}.`,
      status: "empty",
      location: "",
      metadata: "Rows are not inferred from contextual graph neighbors.",
    }];
  }

  return result.rows.map((symbol, index) => ({
    id: `relationship:${result.kind}:${symbol.uid}`,
    order: index,
    uid: symbol.uid,
    primary: symbol.name,
    kind: symbol.kind,
    relationship:
      result.kind === "callers"
        ? `direct caller of ${result.targetLabel}`
        : `direct callee from ${result.targetLabel}`,
    status: result.status,
    location: `${symbol.file_path}:${symbol.start_line}`,
    metadata: [
      symbol.signature,
      symbol.summary,
      symbol.pagerank_score > 0 ? `rank ${symbol.pagerank_score.toFixed(3)}` : null,
    ].filter(Boolean).join(", "),
  }));
}

function backlinkRows(result: BacklinkResultState | null): ResultRow[] | null {
  if (!result) return null;
  if (result.status === "error") {
    return [{
      id: "backlink-error",
      order: 0,
      uid: null,
      primary: "Backlinks unavailable",
      kind: "Backlink state",
      relationship: `Backlinks for ${result.targetLabel} could not be loaded.`,
      status: "error",
      location: "",
      metadata: result.error ?? "The backlinks API returned an error.",
    }];
  }
  if (result.status === "empty" || result.rows.length === 0) {
    return [{
      id: "backlink-empty",
      order: 0,
      uid: null,
      primary: "No backlinks returned",
      kind: "Backlink state",
      relationship: `No notes link to ${result.targetLabel} in the current scope.`,
      status: "empty",
      location: "",
      metadata: "Backlink rows are direct API results, not graph-neighbor inference.",
    }];
  }

  return result.rows.map((row, index) => ({
    id: `backlink:${row.source_note_uid}:${row.source_section_uid || index}`,
    order: index,
    uid: row.source_note_uid,
    primary: row.source_note_title,
    kind: "Note",
    relationship: `direct backlink to ${result.targetLabel}`,
    status: "success",
    location: row.source_note_path,
    metadata: [
      `confidence ${row.confidence.toFixed(2)}`,
      row.display ? `display ${row.display}` : null,
      row.source_section_uid ? `section ${row.source_section_uid}` : null,
    ].filter(Boolean).join(", "),
  }));
}

function traceRows(root: FlowNode | null): ResultRow[] {
  if (!root) {
    return [{
      id: "trace-unavailable",
      order: 0,
      uid: null,
      primary: "Trace unavailable",
      kind: "Trace state",
      relationship: "No trace steps are loaded for this scene.",
      status: "unavailable",
      location: "",
      metadata: "Run a trace-capable symbol to populate ordered steps.",
    }];
  }

  const rows: ResultRow[] = [];
  const visit = (node: FlowNode, parent: FlowNode | null) => {
    rows.push({
      id: `trace:${rows.length}:${node.uid}`,
      order: rows.length,
      uid: node.uid,
      primary: node.name || node.uid,
      kind: "Trace step",
      relationship: parent ? `called from ${parent.name || parent.uid}` : "trace root",
      status: node.file_path ? "evidence available" : "source evidence unavailable",
      location: node.file_path,
      metadata: `depth ${node.depth}, children ${node.children.length}`,
    });
    node.children.forEach((child) => visit(child, node));
  };
  visit(root, null);
  return rows;
}

function pathRows(
  results: ReturnType<typeof useStore.getState>["pathResults"],
  status: ReturnType<typeof useStore.getState>["pathStatus"],
  error: string | null,
): ResultRow[] {
  if (status === "pending") {
    return [{
      id: "path-pending",
      order: 0,
      uid: null,
      primary: "Path query pending",
      kind: "Path state",
      relationship: "Waiting for path results.",
      status,
      location: "",
      metadata: "The selected endpoints are being evaluated.",
    }];
  }
  if (status === "error") {
    return [{
      id: "path-error",
      order: 0,
      uid: null,
      primary: "Path query failed",
      kind: "Path state",
      relationship: "The backend returned an error.",
      status,
      location: "",
      metadata: error ?? "No additional error detail was provided.",
    }];
  }
  if (status === "empty" || results.length === 0) {
    return [{
      id: "path-empty",
      order: 0,
      uid: null,
      primary: status === "empty" ? "No path found" : "Path not run",
      kind: "Path state",
      relationship:
        status === "empty"
          ? "The selected endpoints have no returned path in the current scope."
          : "Choose a destination to run a path query.",
      status,
      location: "",
      metadata: "No path nodes are being hidden.",
    }];
  }

  const rows: ResultRow[] = [];
  results.forEach((result, resultIndex) => {
    result.nodes.forEach((uid, nodeIndex) => {
      const nextEdge = result.edges[nodeIndex];
      rows.push({
        id: `path:${resultIndex}:${nodeIndex}:${uid}`,
        order: rows.length,
        uid,
        primary: uid.split(":").pop() || uid,
        kind: "Path step",
        relationship:
          nodeIndex === result.nodes.length - 1
            ? `path ${resultIndex + 1} destination`
            : `path ${resultIndex + 1} step ${nodeIndex + 1} via ${nextEdge?.type ?? "edge"}`,
        status: "success",
        location: "",
        metadata:
          nextEdge?.confidence != null
            ? `confidence ${nextEdge.confidence.toFixed(2)}, length ${result.length}`
            : `length ${result.length}`,
      });
    });
  });
  return rows;
}

function gapRows(items: ReturnType<typeof useStore.getState>["gapItems"]): ResultRow[] {
  if (items.length === 0) {
    return [{
      id: "gap-empty",
      order: 0,
      uid: null,
      primary: "No gap rows loaded",
      kind: "Limited state",
      relationship: "Dead-code, gap, or unsupported detail is not available in current state.",
      status: "limited",
      location: "",
      metadata: "P1 exposes this as an explicit limited state.",
    }];
  }

  return items.map((item, index) => ({
    id: `gap:${index}:${item.label}`,
    order: index,
    uid: item.nodeUids[0] ?? null,
    primary: item.label,
    kind: item.type,
    relationship: "limited gap/dead-code proxy",
    status: "limited",
    location: "",
    metadata: item.detail,
  }));
}

function unsupportedRows(
  label: string,
  message: string | undefined,
  unsupported: string[],
): ResultRow[] {
  const reasons = unsupported.length > 0 ? unsupported : [message ?? "This result set is unavailable in P1."];
  return reasons.map((reason, index) => ({
    id: `unsupported:${index}`,
    order: index,
    uid: null,
    primary: label,
    kind: "Unsupported or limited",
    relationship: "explicit non-graph state",
    status: "unsupported",
    location: "",
    metadata: reason,
  }));
}

function graphAttribute<T>(graph: GraphInstance, name: string): T | null {
  const value = graph.getAttribute(name) as T | undefined;
  return value ?? null;
}

function affectedTestCount(tests: AffectedTestsResult): number {
  return [...tests.tier_1, ...tests.tier_2, ...tests.tier_3].reduce(
    (count, file) => count + file.tests.length,
    0,
  );
}

function affectedTestRows(
  tests: AffectedTestsResult | null,
  startOrder: number,
  status: string,
): ResultRow[] {
  if (!tests) {
    return [{
      id: "impact-tests-unavailable",
      order: startOrder,
      uid: null,
      primary: "Affected-test hints unavailable",
      kind: "Affected tests",
      relationship: "No affected-test metadata is loaded for this impact result.",
      status,
      location: "",
      metadata: "Impact graph attributes do not include affectedTests.",
    }];
  }

  const rows: ResultRow[] = [{
    id: "impact-tests-summary",
    order: startOrder,
    uid: null,
    primary: "Affected-test summary",
    kind: "Affected tests",
    relationship: tests.summary,
    status,
    location: tests.changed_files.join(", "),
    metadata: tests.disclaimer,
  }];

  const tiers: Array<[keyof Pick<AffectedTestsResult, "tier_1" | "tier_2" | "tier_3">, string]> = [
    ["tier_1", "Tier 1"],
    ["tier_2", "Tier 2"],
    ["tier_3", "Tier 3"],
  ];

  for (const [key, label] of tiers) {
    for (const file of tests[key]) {
      rows.push({
        id: `impact-test:${key}:${file.symbol_uid}:${file.test_file}`,
        order: startOrder + rows.length,
        uid: file.symbol_uid,
        primary: file.test_file,
        kind: "Affected test",
        relationship: `${label} static affected-test hint`,
        status,
        location: file.test_file,
        metadata: [
          file.tests.length > 0 ? `tests ${file.tests.join(", ")}` : null,
          `confidence ${file.confidence.toFixed(2)}`,
        ].filter(Boolean).join(", "),
      });
    }
  }

  if (affectedTestCount(tests) === 0) {
    rows.push({
      id: "impact-tests-empty",
      order: startOrder + rows.length,
      uid: null,
      primary: "No affected tests returned",
      kind: "Affected tests",
      relationship: "Static affected-test analysis returned no scoped test hints.",
      status,
      location: tests.changed_files.join(", "),
      metadata: tests.disclaimer,
    });
  }

  return rows;
}

function impactRows(
  graph: GraphInstance | null,
  activeLensLabel: string,
  targetUid: string | null | undefined,
  trustResult: string,
): ResultRow[] {
  if (!graph) {
    return graphRows(graph, activeLensLabel, targetUid, trustResult);
  }

  const states = graphAttribute<ImpactLensStates>(graph, "impactStates");
  const tests = graphAttribute<AffectedTestsResult>(graph, "affectedTests");
  if (!states && !tests) {
    return graphRows(graph, activeLensLabel, targetUid, trustResult);
  }

  const rows: ResultRow[] = [];
  if (states) {
    ([
      ["tier", "Impact tier", states.tier],
      ["local", "Local impact", states.local],
      ["org", "Org impact", states.org],
      ["freshness", "Freshness", states.freshness],
      ["timeout", "Timeout state", states.timeout],
      ["permission", "Permission state", states.permission],
      ["read_only", "Read-only state", states.read_only],
      ["result", "Result", states.result],
    ] as const).forEach(([key, label, value]) => {
      rows.push({
        id: `impact-state:${key}`,
        order: rows.length,
        uid: null,
        primary: label,
        kind: "Impact state",
        relationship: value,
        status: states.result,
        location: "",
        metadata: key === "org"
          ? "Org-wide and two-tier continuation state from the impact response."
          : "Impact response trust state.",
      });
    });
  }

  rows.push(...affectedTestRows(tests, rows.length, states?.result ?? trustResult));

  if (graph.order > 0) {
    const nodeRows = graphRows(graph, activeLensLabel, targetUid, states?.result ?? trustResult);
    rows.push(...nodeRows.map((row) => ({ ...row, order: rows.length + row.order })));
  }

  return rows;
}

export function NodeListView() {
  const graphInstance = useStore((s) => s.graphInstance);
  const selectedNodeId = useStore((s) => s.selectedNodeId);
  const selectNode = useStore((s) => s.selectNode);
  const exploreNode = useStore((s) => s.exploreNode);
  const activeLens = useStore((s) => s.activeLens);
  const sceneMetadata = useStore((s) => s.sceneMetadata);
  const trustSummary = useStore((s) => s.trustSummary);
  const flowTraceRoot = useStore((s) => s.flowTraceRoot);
  const pathResults = useStore((s) => s.pathResults);
  const pathStatus = useStore((s) => s.pathStatus);
  const pathError = useStore((s) => s.pathError);
  const gapItems = useStore((s) => s.gapItems);
  const gapActive = useStore((s) => s.gapActive);
  const relationshipResult = useStore((s) => s.relationshipResult);
  const backlinkResult = useStore((s) => s.backlinkResult);
  const [filter, setFilter] = useState("");
  const [sortKey, setSortKey] = useState<SortKey>("order");
  const [sortDirection, setSortDirection] = useState<SortDirection>("asc");

  const rows = useMemo<ResultRow[]>(() => {
    const trustResult = trustSummary?.result ?? sceneMetadata?.trust.result ?? "unknown";
    const unsupported = trustSummary?.unsupported ?? sceneMetadata?.trust.unsupported ?? [];
    const lowerLabel = activeLens.label.toLowerCase();
    const directRelationships = relationshipRows(relationshipResult);
    const directBacklinks = backlinkRows(backlinkResult);

    if (activeLens.lens === "trace") return traceRows(flowTraceRoot);
    if (activeLens.lens === "path" || pathStatus !== "idle") {
      return pathRows(pathResults, pathStatus, pathError);
    }
    if (directRelationships && lowerLabel.startsWith(`${relationshipResult?.kind ?? ""} of`)) {
      return directRelationships;
    }
    if (directBacklinks && lowerLabel.startsWith("backlinks for")) {
      return directBacklinks;
    }
    if (gapActive || lowerLabel.includes("dead code")) return gapRows(gapItems);
    if (activeLens.lens === "impact" || lowerLabel.includes("impact")) {
      return impactRows(graphInstance, activeLens.label, activeLens.targetUid, trustResult);
    }
    if (
      activeLens.lens === "unsupported" ||
      lowerLabel.includes("contract drift") ||
      unsupported.length > 0
    ) {
      return unsupportedRows(
        activeLens.label,
        trustSummary?.message ?? sceneMetadata?.trust.message,
        unsupported,
      );
    }

    return graphRows(graphInstance, activeLens.label, activeLens.targetUid, trustResult);
  }, [
    activeLens,
    flowTraceRoot,
    gapActive,
    gapItems,
    graphInstance,
    pathError,
    pathResults,
    pathStatus,
    relationshipResult,
    sceneMetadata,
    trustSummary,
    backlinkResult,
  ]);

  const filtered = useMemo(() => {
    const lower = filter.toLowerCase();
    const next = lower
      ? rows.filter((row) =>
          [
            row.primary,
            row.kind,
            row.relationship,
            row.status,
            row.location,
            row.metadata,
          ].some((value) => value.toLowerCase().includes(lower)),
        )
      : rows;
    return [...next].sort((a, b) => {
      const base = compareValues(a[sortKey], b[sortKey]);
      return sortDirection === "asc" ? base : -base;
    });
  }, [rows, filter, sortKey, sortDirection]);

  const setSort = (key: SortKey) => {
    if (sortKey === key) {
      setSortDirection((current) => (current === "asc" ? "desc" : "asc"));
    } else {
      setSortKey(key);
      setSortDirection(key === "order" || key === "primary" || key === "kind" ? "asc" : "desc");
    }
  };

  const rowButtonSelector = "[data-result-row-button='true']";
  const focusRowButton = (index: number) => {
    const next = document.querySelector<HTMLButtonElement>(
      `${rowButtonSelector}[data-row-index='${index}']`,
    );
    next?.focus();
  };

  const handleRowKeyDown = (
    event: React.KeyboardEvent<HTMLButtonElement>,
    index: number,
    row: ResultRow,
  ) => {
    if (event.key === "ArrowDown") {
      event.preventDefault();
      focusRowButton(Math.min(filtered.length - 1, index + 1));
    } else if (event.key === "ArrowUp") {
      event.preventDefault();
      focusRowButton(Math.max(0, index - 1));
    } else if (event.key === "Enter" && row.uid) {
      event.preventDefault();
      exploreNode(row.uid, row.kind);
    } else if (event.key === " " && row.uid) {
      event.preventDefault();
      selectNode(row.uid, row.kind);
    }
  };

  const ariaSort = (key: SortKey) =>
    sortKey === key
      ? sortDirection === "asc"
        ? "ascending"
        : "descending"
      : "none";

  const header = (key: SortKey, label: string) => (
    <button
      type="button"
      onClick={() => setSort(key)}
      className="inline-flex items-center gap-1 text-[10px] font-semibold uppercase tracking-wide text-[var(--color-text-muted)] hover:text-[var(--color-text)]"
    >
      {label}
      <ArrowDownUp className="h-3 w-3" />
    </button>
  );

  return (
    <div
      className="flex h-full flex-col bg-[var(--color-surface)]"
      role="region"
      aria-label="Result table"
    >
      <div className="flex items-center gap-2 border-b border-[var(--color-border)] p-2">
        <div className="hidden min-w-0 flex-col pr-2 md:flex">
          <span className="truncate text-[11px] font-semibold text-[var(--color-text)]">
            {activeLens.label}
          </span>
          <span className="truncate text-[10px] text-[var(--color-text-muted)]">
            {sceneMetadata?.trust.message ?? "Rows expose the current result-set semantics."}
          </span>
        </div>
        <input
          type="search"
          placeholder="Filter rows by result, relationship, state, or metadata..."
          value={filter}
          onChange={(event) => setFilter(event.target.value)}
          className="h-8 min-w-0 flex-1 rounded border border-[var(--color-border)] bg-[var(--color-surface-alt)] px-2 text-sm text-[var(--color-text)] placeholder:text-[var(--color-text-muted)]"
          aria-label="Filter result rows"
        />
        <span className="shrink-0 text-[11px] text-[var(--color-text-muted)]">
          {filtered.length} of {rows.length}
        </span>
      </div>

      <div className="min-h-0 flex-1 overflow-auto">
        <table
          className="w-full min-w-[1060px] border-collapse text-left text-xs"
          aria-label={`${activeLens.label} result rows`}
        >
          <thead className="sticky top-0 z-10 bg-[var(--color-surface)] shadow-sm">
            <tr className="border-b border-[var(--color-border)]">
              <th scope="col" aria-sort={ariaSort("order")} className="px-3 py-2">{header("order", "#")}</th>
              <th scope="col" aria-sort={ariaSort("primary")} className="px-3 py-2">{header("primary", "Result")}</th>
              <th scope="col" aria-sort={ariaSort("kind")} className="px-3 py-2">{header("kind", "Kind")}</th>
              <th scope="col" aria-sort={ariaSort("relationship")} className="px-3 py-2">{header("relationship", "Semantics")}</th>
              <th scope="col" aria-sort={ariaSort("status")} className="px-3 py-2">{header("status", "State")}</th>
              <th scope="col" aria-sort={ariaSort("location")} className="px-3 py-2">{header("location", "Evidence")}</th>
              <th scope="col" className="whitespace-nowrap px-3 py-2 text-[10px] font-semibold uppercase tracking-wide text-[var(--color-text-muted)]">
                Metadata
              </th>
              <th scope="col" className="whitespace-nowrap px-3 py-2 text-[10px] font-semibold uppercase tracking-wide text-[var(--color-text-muted)]">
                Actions
              </th>
            </tr>
          </thead>
          <tbody>
            {filtered.map((row, index) => (
              <tr
                key={row.id}
                className={`border-b border-[var(--color-border)] ${
                  selectedNodeId === row.uid
                    ? "bg-[var(--color-surface-alt)]"
                    : "hover:bg-[var(--color-surface-alt)]"
                }`}
                aria-selected={selectedNodeId === row.uid}
              >
                <td className="px-3 py-2 text-[var(--color-text-muted)]">
                  {row.order + 1}
                </td>
                <td className="max-w-[220px] px-3 py-2">
                  {row.uid ? (
                    <button
                      type="button"
                      data-result-row-button="true"
                      data-row-index={index}
                      onClick={() => {
                        if (row.uid) selectNode(row.uid, row.kind);
                      }}
                      onDoubleClick={() => {
                        if (row.uid) exploreNode(row.uid, row.kind);
                      }}
                      onKeyDown={(event) => handleRowKeyDown(event, index, row)}
                      className="max-w-full truncate rounded font-medium text-[var(--color-text)] outline-none hover:text-[var(--color-graph-selection)] focus-visible:ring-2 focus-visible:ring-[var(--color-graph-selection)]"
                      aria-label={`${row.primary}, ${row.kind}. Press Space to select, Enter to open, arrows to move rows.`}
                    >
                      {row.primary}
                    </button>
                  ) : (
                    <span className="font-medium text-[var(--color-text)]">{row.primary}</span>
                  )}
                </td>
                <td className="px-3 py-2">
                  <span className="rounded bg-[var(--color-surface-alt)] px-1.5 py-0.5 text-[10px] text-[var(--color-text-muted)]">
                    {row.kind}
                  </span>
                </td>
                <td className="max-w-[260px] px-3 py-2 text-[var(--color-text-muted)]">
                  {row.relationship}
                </td>
                <td className="px-3 py-2 text-[var(--color-text-muted)]">
                  {row.status || "-"}
                </td>
                <td className="max-w-[240px] truncate px-3 py-2 text-[var(--color-text-muted)]">
                  {row.location || "-"}
                </td>
                <td className="max-w-[280px] px-3 py-2 text-[var(--color-text-muted)]">
                  {row.metadata || "-"}
                </td>
                <td className="px-3 py-2">
                  {row.uid ? (
                    <NodeActionBar
                      node={{ uid: row.uid, kind: row.kind, label: row.primary }}
                      ids={["explore", "impact", "path", "ask"]}
                      compact
                    />
                  ) : (
                    <span className="text-[11px] text-[var(--color-text-muted)]">
                      Unavailable
                    </span>
                  )}
                </td>
              </tr>
            ))}
          </tbody>
        </table>
        {filtered.length === 0 && (
          <div className="p-6 text-center text-sm text-[var(--color-text-muted)]">
            No matching result rows
          </div>
        )}
      </div>
    </div>
  );
}
