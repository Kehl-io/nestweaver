import { useStore } from "../../stores";

interface Props {
  open: boolean;
}

export function ForceControls({ open }: Props) {
  const forceParams = useStore((s) => s.forceParams);
  const setForceParams = useStore((s) => s.setForceParams);

  if (!open) return null;

  return (
    <div
      className="w-[200px] rounded border border-[var(--color-border)] bg-[var(--color-surface)] p-3 shadow-md"
      style={{ fontSize: "11px" }}
    >
      <div className="mb-2 font-semibold text-[var(--color-text-muted)] uppercase tracking-wide" style={{ fontSize: "10px" }}>
        Force Physics
      </div>

      {/* Force Scale */}
      <div className="mb-2">
        <div className="flex justify-between text-[var(--color-text-muted)] mb-0.5">
          <span title="Higher values spread nodes further apart">Force Scale</span>
          <span>{forceParams.repulsion.toFixed(1)}</span>
        </div>
        <input
          type="range"
          min={0.5}
          max={5.0}
          step={0.1}
          value={forceParams.repulsion}
          onChange={(e) => setForceParams({ repulsion: parseFloat(e.target.value) })}
          className="w-full h-1.5 cursor-pointer"
          style={{ accentColor: "var(--color-accent, #3b82f6)" }}
        />
      </div>

      {/* Gravity */}
      <div className="mb-2">
        <div className="flex justify-between text-[var(--color-text-muted)] mb-0.5">
          <span>Gravity</span>
          <span>{forceParams.gravity.toFixed(1)}</span>
        </div>
        <input
          type="range"
          min={0.1}
          max={3.0}
          step={0.1}
          value={forceParams.gravity}
          onChange={(e) => setForceParams({ gravity: parseFloat(e.target.value) })}
          className="w-full h-1.5 cursor-pointer"
          style={{ accentColor: "var(--color-accent, #3b82f6)" }}
        />
      </div>

      {/* Damping */}
      <div>
        <div className="flex justify-between text-[var(--color-text-muted)] mb-0.5">
          <span title="Higher values slow convergence for more stable layouts">Damping</span>
          <span>{forceParams.settling}</span>
        </div>
        <input
          type="range"
          min={1}
          max={20}
          step={1}
          value={forceParams.settling}
          onChange={(e) => setForceParams({ settling: parseInt(e.target.value, 10) })}
          className="w-full h-1.5 cursor-pointer"
          style={{ accentColor: "var(--color-accent, #3b82f6)" }}
        />
      </div>
    </div>
  );
}
