import type { PreviewData } from "../../hooks/useNodePreview";
import type { SceneMetadata } from "../../api/p1Types";

interface JsonEvidenceProps {
  nodeId: string;
  data: PreviewData;
  metadata?: SceneMetadata | null;
}

export function JsonEvidence({ nodeId, data, metadata }: JsonEvidenceProps) {
  const payload = {
    node: nodeId,
    preview: data,
    _meta: metadata ?? null,
  };

  return (
    <section className="border-t border-[var(--color-border)] p-3">
      <div className="mb-2 flex items-center justify-between gap-2">
        <h3 className="text-[11px] font-semibold uppercase tracking-wide text-[var(--color-text-muted)]">
          JSON evidence
        </h3>
        <span className="text-[10px] text-[var(--color-text-muted)]">
          Preview payload
        </span>
      </div>
      <pre className="max-h-48 overflow-auto rounded border border-[var(--color-border)] bg-[var(--color-surface-alt)] p-2 text-[10px] leading-4 text-[var(--color-text)]">
        {JSON.stringify(payload, null, 2)}
      </pre>
    </section>
  );
}
