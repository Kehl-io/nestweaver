import type { PreviewData } from "../../hooks/useNodePreview";

interface RelationshipChipsProps {
  data: PreviewData;
  fallbackKind?: string | null;
}

function chip(label: string, value: number | string, title: string) {
  return (
    <span
      key={label}
      className="inline-flex h-6 items-center gap-1 rounded border border-[var(--color-border)] bg-[var(--color-surface-alt)] px-2 text-[11px] text-[var(--color-text-muted)]"
      title={title}
    >
      <span className="font-semibold text-[var(--color-text)]">{value}</span>
      <span>{label}</span>
    </span>
  );
}

export function RelationshipChips({ data, fallbackKind }: RelationshipChipsProps) {
  if (!data) {
    return (
      <div className="flex flex-wrap gap-1.5" aria-label="Relationship summary">
        {chip("kind", fallbackKind ?? "unknown", "Preview loaded from graph metadata only.")}
      </div>
    );
  }

  if (data.type === "symbol") {
    const { callers, callees } = data.detail;
    return (
      <div className="flex flex-wrap gap-1.5" aria-label="Relationship summary">
        {chip("callers", callers.length, "Symbols that call this item.")}
        {chip("callees", callees.length, "Symbols called by this item.")}
        {chip("refs", callers.length + callees.length, "Total caller and callee relationships.")}
      </div>
    );
  }

  const { headings, sections, note } = data.detail;
  return (
    <div className="flex flex-wrap gap-1.5" aria-label="Relationship summary">
      {chip("headings", headings.length, "Headings in this note.")}
      {chip("sections", sections.length, "Indexed sections in this note.")}
      {chip("words", note.word_count.toLocaleString(), "Indexed note word count.")}
    </div>
  );
}
