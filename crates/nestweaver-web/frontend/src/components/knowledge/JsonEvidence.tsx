import type { PreviewData } from "../../hooks/useNodePreview";
import type { SceneMetadata } from "../../api/p1Types";
import type { Symbol } from "../../api/types";

interface JsonEvidenceProps {
  nodeId: string;
  data: PreviewData;
  metadata?: SceneMetadata | null;
}

function trimText(value: string | null | undefined, max = 360): string | null {
  if (!value) return null;
  const normalized = value.replace(/\s+/g, " ").trim();
  return normalized.length > max ? `${normalized.slice(0, max)}...` : normalized;
}

function summarizeSymbol(symbol: Symbol) {
  return {
    uid: symbol.uid,
    name: symbol.name,
    kind: symbol.kind,
    repo_uid: symbol.repo_uid,
    file_path: symbol.file_path,
    start_line: symbol.start_line,
    signature: trimText(symbol.signature, 240),
    summary: trimText(symbol.summary, 360),
    pagerank_score: symbol.pagerank_score,
  };
}

function sanitizedPreview(data: PreviewData) {
  if (!data) return null;

  if (data.type === "symbol") {
    return {
      type: "symbol",
      symbol: summarizeSymbol(data.detail.symbol),
      source_excerpt: data.sourceLines.slice(0, 8),
      relationships: {
        callers_count: data.detail.callers.length,
        callees_count: data.detail.callees.length,
        callers: data.detail.callers.slice(0, 12).map(summarizeSymbol),
        callees: data.detail.callees.slice(0, 12).map(summarizeSymbol),
      },
    };
  }

  return {
    type: "note",
    note: data.detail.note,
    body_excerpt: trimText(data.detail.body, 500),
    headings: data.detail.headings.map((heading) => ({
      uid: heading.uid,
      level: heading.level,
      text: heading.text,
      slug: heading.slug,
      start_line: heading.start_line,
      end_line: heading.end_line,
    })),
    sections: data.detail.sections.slice(0, 24).map((section) => ({
      uid: section.uid,
      heading_uid: section.heading_uid,
      start_line: section.start_line,
      end_line: section.end_line,
      word_count: section.word_count,
      pagerank_score: section.pagerank_score,
    })),
    section_count: data.detail.sections.length,
  };
}

export function JsonEvidence({ nodeId, data, metadata }: JsonEvidenceProps) {
  const payload = {
    node: nodeId,
    preview: sanitizedPreview(data),
    _meta: metadata ?? null,
  };

  return (
    <section className="border-t border-[var(--color-border)] p-3">
      <div className="mb-2 flex items-center justify-between gap-2">
        <h3 className="text-[11px] font-semibold uppercase tracking-wide text-[var(--color-text-muted)]">
          JSON evidence
        </h3>
        <span className="text-[10px] text-[var(--color-text-muted)]">
          Sanitized preview
        </span>
      </div>
      <pre className="max-h-48 overflow-auto rounded border border-[var(--color-border)] bg-[var(--color-surface-alt)] p-2 text-[10px] leading-4 text-[var(--color-text)]">
        {JSON.stringify(payload, null, 2)}
      </pre>
    </section>
  );
}
