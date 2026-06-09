import { useMemo, useState } from "react";
import { EDGE_COLORS, kindColor } from "./utils/graphColors";
import { useStore } from "../../stores";
import { NodeActionBar } from "../actions/NodeActionBar";

const MATRIX_LIMIT = 80;

interface MatrixNode {
  uid: string;
  name: string;
  kind: string;
  relevance: number;
  degree: number;
}

interface MatrixEdge {
  source: string;
  target: string;
  type: string;
  confidence?: number;
}

interface SelectedCell {
  source: MatrixNode;
  target: MatrixNode;
  edge: MatrixEdge;
}

function edgeKey(source: string, target: string): string {
  return `${source}\u0000${target}`;
}

export function GraphMatrixView() {
  const graphInstance = useStore((s) => s.graphInstance);
  const selectedNodeId = useStore((s) => s.selectedNodeId);
  const selectNode = useStore((s) => s.selectNode);
  const isDark = document.documentElement.classList.contains("dark");
  const [selectedCell, setSelectedCell] = useState<SelectedCell | null>(null);

  const { nodes, edges, total } = useMemo(() => {
    if (!graphInstance) {
      return {
        nodes: [] as MatrixNode[],
        edges: new Map<string, MatrixEdge>(),
        total: 0,
      };
    }

    const allNodes: MatrixNode[] = [];
    graphInstance.forEachNode((uid, attrs) => {
      allNodes.push({
        uid,
        name: (attrs.label as string) || uid.split(":").pop() || uid,
        kind: (attrs.kind as string) || "Unknown",
        relevance: (attrs.relevance as number) || 0,
        degree: graphInstance.degree(uid),
      });
    });

    allNodes.sort((a, b) => {
      if (b.relevance !== a.relevance) return b.relevance - a.relevance;
      if (b.degree !== a.degree) return b.degree - a.degree;
      return a.name.localeCompare(b.name);
    });

    const visible = allNodes.slice(0, MATRIX_LIMIT);
    const visibleIds = new Set(visible.map((node) => node.uid));
    const edgeMap = new Map<string, MatrixEdge>();

    graphInstance.forEachEdge((_edge, attrs, source, target) => {
      if (!visibleIds.has(source) || !visibleIds.has(target)) return;
      const type =
        (attrs.edgeType as string | undefined) ||
        (attrs.label as string | undefined) ||
        "edge";
      edgeMap.set(edgeKey(source, target), {
        source,
        target,
        type,
        confidence: attrs.confidence as number | undefined,
      });
    });

    visible.sort((a, b) => {
      const kind = a.kind.localeCompare(b.kind);
      if (kind !== 0) return kind;
      if (b.relevance !== a.relevance) return b.relevance - a.relevance;
      return a.name.localeCompare(b.name);
    });

    return { nodes: visible, edges: edgeMap, total: allNodes.length };
  }, [graphInstance]);

  if (!graphInstance || nodes.length === 0) {
    return (
      <div className="flex h-full items-center justify-center bg-[var(--color-surface)] text-sm text-[var(--color-text-muted)]">
        No graph loaded for matrix view.
      </div>
    );
  }

  return (
    <div
      className="flex h-full flex-col bg-[var(--color-surface)] text-xs"
      role="region"
      aria-label="Graph matrix view"
    >
      <div className="flex items-center justify-between gap-3 border-b border-[var(--color-border)] px-3 py-2">
        <div>
          <h2 className="text-sm font-semibold text-[var(--color-text)]">
            Matrix
          </h2>
          <p className="text-[11px] text-[var(--color-text-muted)]">
            Showing top {nodes.length} of {total} ranked nodes
          </p>
        </div>
        {selectedCell && (
          <div className="max-w-[520px] rounded border border-[var(--color-border)] bg-[var(--color-surface-alt)] p-2">
            <p className="truncate text-[11px] text-[var(--color-text)]">
              {selectedCell.source.name} {"->"} {selectedCell.target.name}
            </p>
            <p className="text-[10px] text-[var(--color-text-muted)]">
              {selectedCell.edge.type}
              {selectedCell.edge.confidence != null
                ? `, confidence ${selectedCell.edge.confidence.toFixed(2)}`
                : ""}
            </p>
            <NodeActionBar
              node={{
                uid: selectedCell.source.uid,
                kind: selectedCell.source.kind,
                label: selectedCell.source.name,
              }}
              ids={["explore", "impact", "path", "ask"]}
              compact
              className="mt-2"
            />
          </div>
        )}
      </div>

      <div className="min-h-0 flex-1 overflow-auto">
        <table className="border-collapse">
          <thead>
            <tr>
              <th className="sticky left-0 top-0 z-30 h-28 w-44 border border-[var(--color-border)] bg-[var(--color-surface)]" />
              {nodes.map((node) => (
                <th
                  key={node.uid}
                  className="sticky top-0 z-20 h-28 min-w-8 border border-[var(--color-border)] bg-[var(--color-surface)] p-1 align-bottom"
                >
                  <button
                    type="button"
                    onClick={() => selectNode(node.uid, node.kind)}
                    className="mx-auto block max-h-24 max-w-7 truncate text-left text-[10px] text-[var(--color-text-muted)] hover:text-[var(--color-text)]"
                    style={{ writingMode: "vertical-rl", textOrientation: "mixed" }}
                    title={node.name}
                  >
                    {node.name}
                  </button>
                </th>
              ))}
            </tr>
          </thead>
          <tbody>
            {nodes.map((row) => (
              <tr key={row.uid}>
                <th className="sticky left-0 z-10 max-w-44 border border-[var(--color-border)] bg-[var(--color-surface)] px-2 py-1 text-left">
                  <button
                    type="button"
                    onClick={() => selectNode(row.uid, row.kind)}
                    className={`flex w-full items-center gap-1.5 truncate ${
                      selectedNodeId === row.uid
                        ? "text-[var(--color-graph-selection)]"
                        : "text-[var(--color-text)] hover:text-[var(--color-graph-selection)]"
                    }`}
                    title={row.name}
                  >
                    <span
                      className="h-2.5 w-2.5 shrink-0 rounded-full"
                      style={{ backgroundColor: kindColor(row.kind, isDark) }}
                    />
                    <span className="truncate">{row.name}</span>
                  </button>
                </th>
                {nodes.map((column) => {
                  const edge = edges.get(edgeKey(row.uid, column.uid));
                  return (
                    <td
                      key={column.uid}
                      className="h-7 w-7 border border-[var(--color-border)] p-0"
                    >
                      {edge ? (
                        <button
                          type="button"
                          title={`${row.name} -> ${column.name}: ${edge.type}`}
                          onClick={() => {
                            selectNode(row.uid, row.kind);
                            setSelectedCell({ source: row, target: column, edge });
                          }}
                          className="h-full w-full transition-transform hover:scale-110"
                          style={{
                            backgroundColor: EDGE_COLORS[edge.type] ?? "#94a3b8",
                            opacity:
                              edge.confidence != null
                                ? Math.max(0.3, edge.confidence)
                                : 0.85,
                          }}
                        />
                      ) : (
                        <span className="block h-full w-full" />
                      )}
                    </td>
                  );
                })}
              </tr>
            ))}
          </tbody>
        </table>
      </div>
    </div>
  );
}
