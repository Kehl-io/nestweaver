import { useStore } from "../../stores";
import type { FlowNode } from "../../stores/analysisSlice";

function FlowNodeItem({ node, depth }: { node: FlowNode; depth: number }) {
  const selectNode = useStore((s) => s.selectNode);
  return (
    <div>
      <button
        onClick={() => selectNode(node.uid, "Function")}
        className="w-full text-left py-1 text-xs hover:bg-[var(--color-surface-alt)] flex items-center gap-1"
        style={{ paddingLeft: `${depth * 16 + 4}px` }}
      >
        <span className="text-[var(--color-text-muted)] font-mono w-4 text-right shrink-0">
          {depth + 1}
        </span>
        <span className="truncate font-medium">{node.name}</span>
        <span className="text-[10px] text-[var(--color-text-muted)] ml-auto truncate max-w-20">
          {node.file_path.split("/").pop()}
        </span>
      </button>
      {node.children.map((child) => (
        <FlowNodeItem key={child.uid} node={child} depth={depth + 1} />
      ))}
    </div>
  );
}

export function FlowDetail() {
  const flowTraceRoot = useStore((s) => s.flowTraceRoot);
  const clearFlowTrace = useStore((s) => s.clearFlowTrace);
  if (!flowTraceRoot) return null;

  function countNodes(n: FlowNode): number {
    return 1 + n.children.reduce((sum, c) => sum + countNodes(c), 0);
  }
  function maxDepth(n: FlowNode): number {
    if (n.children.length === 0) return 0;
    return 1 + Math.max(...n.children.map(maxDepth));
  }

  return (
    <div className="p-3 text-sm border-b border-[var(--color-border)]">
      <div className="flex items-center justify-between mb-2">
        <h3 className="font-semibold text-xs uppercase text-[var(--color-text-muted)]">
          Flow Trace
        </h3>
        <button
          onClick={clearFlowTrace}
          className="text-xs text-blue-500 hover:underline"
        >
          Clear
        </button>
      </div>
      <div className="text-xs text-[var(--color-text-muted)] mb-2">
        {countNodes(flowTraceRoot)} steps, depth {maxDepth(flowTraceRoot)}
      </div>
      <FlowNodeItem node={flowTraceRoot} depth={0} />
    </div>
  );
}
