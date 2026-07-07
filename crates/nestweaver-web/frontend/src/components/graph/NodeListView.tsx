import { useMemo, useState } from "react";
import { ArrowDownUp } from "lucide-react";
import { useStore } from "../../stores";
import { NodeActionBar } from "../actions/NodeActionBar";

type SortKey = "name" | "kind" | "relevance" | "degree" | "location";
type SortDirection = "asc" | "desc";

interface TableNode {
  uid: string;
  name: string;
  kind: string;
  location: string;
  relevance: number;
  degree: number;
}

function compareValues(a: string | number, b: string | number): number {
  if (typeof a === "number" && typeof b === "number") return a - b;
  return String(a).localeCompare(String(b));
}

export function NodeListView() {
  const graphInstance = useStore((s) => s.graphInstance);
  const selectedNodeId = useStore((s) => s.selectedNodeId);
  const selectNode = useStore((s) => s.selectNode);
  const exploreNode = useStore((s) => s.exploreNode);
  const activeLens = useStore((s) => s.activeLens);
  const sceneMetadata = useStore((s) => s.sceneMetadata);
  const [filter, setFilter] = useState("");
  const [sortKey, setSortKey] = useState<SortKey>("relevance");
  const [sortDirection, setSortDirection] = useState<SortDirection>("desc");

  const nodes = useMemo<TableNode[]>(() => {
    if (!graphInstance) return [];
    const list: TableNode[] = [];
    graphInstance.forEachNode((uid, attrs) => {
      list.push({
        uid,
        name: (attrs.label as string) || uid.split(":").pop() || uid,
        kind: (attrs.kind as string) || "Unknown",
        location:
          (attrs.location as string) ||
          (attrs.filePath as string) ||
          "",
        relevance: (attrs.relevance as number) || 0,
        degree: graphInstance.degree(uid),
      });
    });
    return list;
  }, [graphInstance]);

  const filtered = useMemo(() => {
    const lower = filter.toLowerCase();
    const next = lower
      ? nodes.filter(
          (node) =>
            node.name.toLowerCase().includes(lower) ||
            node.kind.toLowerCase().includes(lower) ||
            node.location.toLowerCase().includes(lower),
        )
      : nodes;
    return [...next].sort((a, b) => {
      const base = compareValues(a[sortKey], b[sortKey]);
      return sortDirection === "asc" ? base : -base;
    });
  }, [nodes, filter, sortKey, sortDirection]);

  const setSort = (key: SortKey) => {
    if (sortKey === key) {
      setSortDirection((current) => (current === "asc" ? "desc" : "asc"));
    } else {
      setSortKey(key);
      setSortDirection(key === "name" || key === "kind" ? "asc" : "desc");
    }
  };

  const rowButtonSelector = "[data-node-row-button='true']";
  const focusRowButton = (index: number) => {
    const next = document.querySelector<HTMLButtonElement>(
      `${rowButtonSelector}[data-row-index='${index}']`,
    );
    next?.focus();
  };

  const handleRowKeyDown = (
    event: React.KeyboardEvent<HTMLButtonElement>,
    index: number,
    node: TableNode,
  ) => {
    if (event.key === "ArrowDown") {
      event.preventDefault();
      focusRowButton(Math.min(filtered.length - 1, index + 1));
    } else if (event.key === "ArrowUp") {
      event.preventDefault();
      focusRowButton(Math.max(0, index - 1));
    } else if (event.key === "Enter") {
      event.preventDefault();
      exploreNode(node.uid, node.kind);
    } else if (event.key === " ") {
      event.preventDefault();
      selectNode(node.uid, node.kind);
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
      aria-label="Ranked node table"
    >
      <div className="flex items-center gap-2 border-b border-[var(--color-border)] p-2">
        <div className="hidden min-w-0 flex-col pr-2 md:flex">
          <span className="truncate text-[11px] font-semibold text-[var(--color-text)]">
            {activeLens.label}
          </span>
          <span className="truncate text-[10px] text-[var(--color-text-muted)]">
            {sceneMetadata?.trust.message ?? "Rows mirror the current graph result."}
          </span>
        </div>
        <input
          type="search"
          placeholder="Filter nodes by name, kind, or location..."
          value={filter}
          onChange={(event) => setFilter(event.target.value)}
          className="h-8 min-w-0 flex-1 rounded border border-[var(--color-border)] bg-[var(--color-surface-alt)] px-2 text-sm text-[var(--color-text)] placeholder:text-[var(--color-text-muted)]"
          aria-label="Filter nodes"
        />
        <span className="shrink-0 text-[11px] text-[var(--color-text-muted)]">
          {filtered.length} of {nodes.length}
        </span>
      </div>

      <div className="min-h-0 flex-1 overflow-auto">
        <table
          className="w-full min-w-[760px] border-collapse text-left text-xs"
          aria-label={`${activeLens.label} node results`}
        >
          <thead className="sticky top-0 z-10 bg-[var(--color-surface)] shadow-sm">
            <tr className="border-b border-[var(--color-border)]">
              <th scope="col" aria-sort={ariaSort("name")} className="px-3 py-2">{header("name", "Name")}</th>
              <th scope="col" aria-sort={ariaSort("kind")} className="px-3 py-2">{header("kind", "Kind")}</th>
              <th scope="col" aria-sort={ariaSort("relevance")} className="px-3 py-2">{header("relevance", "Rank")}</th>
              <th scope="col" aria-sort={ariaSort("degree")} className="px-3 py-2">{header("degree", "Degree")}</th>
              <th scope="col" aria-sort={ariaSort("location")} className="px-3 py-2">{header("location", "Location")}</th>
              <th scope="col" className="px-3 py-2 text-[10px] font-semibold uppercase tracking-wide text-[var(--color-text-muted)]">
                Actions
              </th>
            </tr>
          </thead>
          <tbody>
            {filtered.map((node, index) => (
              <tr
                key={node.uid}
                className={`border-b border-[var(--color-border)] ${
                  selectedNodeId === node.uid
                    ? "bg-[var(--color-surface-alt)]"
                    : "hover:bg-[var(--color-surface-alt)]"
                }`}
                aria-selected={selectedNodeId === node.uid}
              >
                <td className="max-w-[240px] px-3 py-2">
                  <button
                    type="button"
                    data-node-row-button="true"
                    data-row-index={index}
                    onClick={() => selectNode(node.uid, node.kind)}
                    onDoubleClick={() => exploreNode(node.uid, node.kind)}
                    onKeyDown={(event) => handleRowKeyDown(event, index, node)}
                    className="max-w-full truncate rounded font-medium text-[var(--color-text)] outline-none hover:text-[var(--color-graph-selection)] focus-visible:ring-2 focus-visible:ring-[var(--color-graph-selection)]"
                    aria-label={`${node.name}, ${node.kind}. Press Space to select, Enter to open, arrows to move rows.`}
                  >
                    {node.name}
                  </button>
                </td>
                <td className="px-3 py-2">
                  <span className="rounded bg-[var(--color-surface-alt)] px-1.5 py-0.5 text-[10px] text-[var(--color-text-muted)]">
                    {node.kind}
                  </span>
                </td>
                <td className="px-3 py-2 text-[var(--color-text-muted)]">
                  {node.relevance > 0 ? node.relevance.toFixed(3) : "-"}
                </td>
                <td className="px-3 py-2 text-[var(--color-text-muted)]">
                  {node.degree}
                </td>
                <td className="max-w-[260px] truncate px-3 py-2 text-[var(--color-text-muted)]">
                  {node.location || "-"}
                </td>
                <td className="px-3 py-2">
                  <NodeActionBar
                    node={{ uid: node.uid, kind: node.kind, label: node.name }}
                    ids={["explore", "impact", "path", "ask"]}
                    compact
                  />
                </td>
              </tr>
            ))}
          </tbody>
        </table>
        {filtered.length === 0 && (
          <div className="p-6 text-center text-sm text-[var(--color-text-muted)]">
            {nodes.length === 0 ? "No graph loaded" : "No matching nodes"}
          </div>
        )}
      </div>
    </div>
  );
}
