import { useStore } from "../../stores";
import { api } from "../../api/client";

interface Props {
  x: number;
  y: number;
  nodeId: string;
  onClose: () => void;
}

export function ContextMenu({ x, y, nodeId, onClose }: Props) {
  const setSeeds = useStore((s) => s.setSeeds);
  const setGraphMode = useStore((s) => s.setGraphMode);
  const selectNode = useStore((s) => s.selectNode);
  const setFlowTrace = useStore((s) => s.setFlowTrace);
  const startPathfinding = useStore((s) => s.startPathfinding);

  const actions = [
    { label: "Re-seed context from here", action: () => { setSeeds([nodeId]); onClose(); } },
    { label: "Impact analysis", action: () => { selectNode(nodeId, null); setGraphMode("impact"); onClose(); } },
    {
      label: "Trace flow from here",
      action: async () => {
        try {
          const result = await api.flow(nodeId, 10);
          setFlowTrace(result as any);
        } catch { /* ignore */ }
        onClose();
      },
    },
    {
      label: "Find path to...",
      action: () => { startPathfinding(nodeId); onClose(); },
    },
    { label: "Copy UID", action: () => { navigator.clipboard.writeText(nodeId); onClose(); } },
  ];

  return (
    <div role="menu" onKeyDown={(e) => { if (e.key === "Escape") onClose(); }}
         className="fixed z-50 bg-[var(--color-surface)] border border-[var(--color-border)] rounded-lg shadow-lg py-1 min-w-40"
         style={{ left: x, top: y }}>
      {actions.map((a) => (
        <button role="menuitem" key={a.label} onClick={a.action}
                className="w-full text-left px-3 py-1.5 text-xs hover:bg-[var(--color-surface-alt)]">
          {a.label}
        </button>
      ))}
    </div>
  );
}
