import { useState, useMemo, useCallback } from "react";
import { useStore } from "../../stores";

export function NodeListView() {
  const graphInstance = useStore((s) => s.graphInstance);
  const selectNode = useStore((s) => s.selectNode);
  const selectedNodeId = useStore((s) => s.selectedNodeId);
  const exploreNode = useStore((s) => s.exploreNode);
  const [filter, setFilter] = useState("");

  // Build sorted list from graphology
  const nodes = useMemo(() => {
    if (!graphInstance) return [];
    const list: Array<{
      uid: string;
      name: string;
      kind: string;
      location: string;
      relevance: number;
    }> = [];
    graphInstance.forEachNode((uid, attrs) => {
      list.push({
        uid,
        name: (attrs.label as string) || uid.split(":").pop() || uid,
        kind: (attrs.kind as string) || "Unknown",
        location: (attrs.location as string) || "",
        relevance: (attrs.relevance as number) || 0,
      });
    });
    // Sort by relevance descending (proxy for PageRank)
    list.sort((a, b) => b.relevance - a.relevance);
    return list;
  }, [graphInstance]);

  const filtered = useMemo(() => {
    if (!filter) return nodes;
    const lower = filter.toLowerCase();
    return nodes.filter(
      (n) => n.name.toLowerCase().includes(lower) || n.kind.toLowerCase().includes(lower),
    );
  }, [nodes, filter]);

  const handleKeyDown = useCallback(
    (e: React.KeyboardEvent, uid: string, kind: string) => {
      if (e.key === "Enter") {
        selectNode(uid);
      } else if (e.key === " ") {
        e.preventDefault();
        exploreNode(uid, kind);
      }
    },
    [selectNode, exploreNode],
  );

  return (
    <div
      className="flex flex-col h-full bg-[var(--color-surface)]"
      role="region"
      aria-label="Node list view"
    >
      <div className="p-2 border-b border-[var(--color-border)]">
        <input
          type="search"
          placeholder="Filter nodes..."
          value={filter}
          onChange={(e) => setFilter(e.target.value)}
          className="w-full px-2 py-1 text-sm rounded border border-[var(--color-border)] bg-[var(--color-surface-alt)] text-[var(--color-text)] placeholder:text-[var(--color-text-muted)]"
          aria-label="Filter nodes"
        />
      </div>
      <div className="flex-1 overflow-y-auto" role="listbox" aria-label="Graph nodes">
        {filtered.map((node) => (
          <div
            key={node.uid}
            role="option"
            aria-selected={selectedNodeId === node.uid}
            tabIndex={0}
            onClick={() => selectNode(node.uid)}
            onDoubleClick={() => exploreNode(node.uid, node.kind)}
            onKeyDown={(e) => handleKeyDown(e, node.uid, node.kind)}
            className={`flex items-center gap-2 px-3 py-1.5 text-xs cursor-pointer border-b border-[var(--color-border)] hover:bg-[var(--color-surface-alt)] ${
              selectedNodeId === node.uid
                ? "bg-blue-500/10 border-l-2 border-l-blue-500"
                : ""
            }`}
          >
            <span className="font-mono text-[10px] px-1 rounded bg-[var(--color-surface-alt)] text-[var(--color-text-muted)]">
              {node.kind}
            </span>
            <span className="flex-1 truncate text-[var(--color-text)]">{node.name}</span>
            <span className="text-[var(--color-text-muted)] text-[10px]">
              {node.relevance > 0 ? node.relevance.toFixed(3) : ""}
            </span>
          </div>
        ))}
        {filtered.length === 0 && (
          <div className="p-4 text-center text-sm text-[var(--color-text-muted)]">
            {nodes.length === 0 ? "No graph loaded" : "No matching nodes"}
          </div>
        )}
      </div>
      <div className="px-3 py-1 text-[10px] text-[var(--color-text-muted)] border-t border-[var(--color-border)]">
        {filtered.length} node{filtered.length !== 1 ? "s" : ""} · Enter to select · Space to
        explore
      </div>
    </div>
  );
}
