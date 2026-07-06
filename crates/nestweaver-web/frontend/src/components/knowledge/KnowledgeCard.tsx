import type { ReactNode } from "react";
import { X } from "lucide-react";
import { KindBadge } from "../shared/KindBadge";
import type { PreviewData } from "../../hooks/useNodePreview";
import type { SceneMetadata, TrustSummary } from "../../api/p1Types";
import { KnowledgeActionGrid } from "./KnowledgeActionGrid";
import { RelationshipChips } from "./RelationshipChips";
import { TrustBadge } from "./TrustBadge";
import { JsonEvidence } from "./JsonEvidence";

interface KnowledgeCardNode {
  uid: string;
  label: string;
  kind: string;
  location?: string | null;
}

interface KnowledgeCardProps {
  node: KnowledgeCardNode;
  data: PreviewData;
  loading?: boolean;
  error?: string | null;
  expanded?: boolean;
  metadata?: SceneMetadata | null;
  trustSummary?: TrustSummary | null;
  onClose: () => void;
  onToggleExpanded: () => void;
  children?: ReactNode;
}

function stripFrontmatterAndHeadings(body: string): string {
  let text = body.replace(/^---[\s\S]*?---\n?/, "");
  text = text.replace(/^#{1,6}\s+.*/gm, "");
  return text.replace(/\n{3,}/g, "\n\n").trim();
}

function excerptFor(data: PreviewData, fallback: KnowledgeCardNode): string {
  if (!data) {
    return fallback.location
      ? `Graph metadata places this item at ${fallback.location}.`
      : "Preview detail is not available yet; graph metadata is still usable.";
  }

  if (data.type === "symbol") {
    if (data.sourceLines.length > 0) return data.sourceLines.slice(0, 5).join("\n");
    return data.detail.symbol.summary ?? "No source excerpt is available for this symbol.";
  }

  const preview = stripFrontmatterAndHeadings(data.detail.body);
  return preview || "No note excerpt is available.";
}

function roleFor(data: PreviewData, fallback: KnowledgeCardNode): string {
  if (!data) return fallback.kind;
  if (data.type === "symbol") {
    const signature = data.detail.symbol.signature;
    return signature ? signature : `${data.detail.symbol.kind} in ${data.detail.symbol.repo_uid}`;
  }
  return `${data.detail.note.note_kind} note`;
}

function locationFor(data: PreviewData, fallback: KnowledgeCardNode): string {
  if (!data) return fallback.location ?? fallback.uid;
  if (data.type === "symbol") {
    return `${data.detail.symbol.file_path}:${data.detail.symbol.start_line}`;
  }
  return data.detail.note.file_path;
}

export function KnowledgeCard({
  node,
  data,
  loading = false,
  error = null,
  expanded = false,
  metadata,
  trustSummary,
  onClose,
  onToggleExpanded,
  children,
}: KnowledgeCardProps) {
  const nodeContext = {
    uid: node.uid,
    kind: data?.type === "symbol" ? data.detail.symbol.kind : data?.type === "note" ? "note" : node.kind,
    label: data?.type === "symbol" ? data.detail.symbol.name : data?.type === "note" ? data.detail.note.title : node.label,
  };
  const state = loading ? "loading" : error ? "error" : data ? "ready" : "empty";
  const excerpt = excerptFor(data, node);
  const role = roleFor(data, node);
  const location = locationFor(data, node);
  const isCode = data?.type === "symbol" && data.sourceLines.length > 0;

  return (
    <article className="flex min-h-0 flex-1 flex-col overflow-hidden bg-[var(--color-surface)] text-[var(--color-text)]">
      <header className="shrink-0 border-b border-[var(--color-border)] px-3 py-2">
        <div className="flex items-start justify-between gap-2">
          <div className="min-w-0">
            <div className="flex min-w-0 items-center gap-1.5">
              <KindBadge kind={nodeContext.kind ?? "Unknown"} />
              <h2 className="truncate text-sm font-semibold">
                {nodeContext.label}
              </h2>
            </div>
            <p className="mt-1 truncate text-[11px] text-[var(--color-text-muted)]">
              {location}
            </p>
          </div>
          <button
            type="button"
            onClick={onClose}
            className="inline-flex h-7 w-7 shrink-0 items-center justify-center rounded border border-transparent text-[var(--color-text-muted)] transition-colors hover:border-[var(--color-border)] hover:bg-[var(--color-surface-alt)] hover:text-[var(--color-text)]"
            aria-label="Close preview"
            title="Close preview"
          >
            <X className="h-3.5 w-3.5" />
          </button>
        </div>
        <div className="mt-2 flex flex-wrap items-center gap-1.5">
          <TrustBadge metadata={metadata} summary={trustSummary} state={state} />
          {loading && (
            <span className="text-[11px] text-[var(--color-text-muted)]">
              Fetching preview detail
            </span>
          )}
          {error && (
            <span className="text-[11px] text-red-300" title={error}>
              Preview unavailable
            </span>
          )}
        </div>
      </header>

      <div className="min-h-0 flex-1 overflow-auto px-3 py-2">
        <section>
          <h3 className="text-[10px] font-semibold uppercase tracking-wide text-[var(--color-text-muted)]">
            Role
          </h3>
          <p className="mt-1 break-words text-xs leading-5 text-[var(--color-text-muted)]">
            {role}
          </p>
        </section>

        <section className="mt-3">
          <h3 className="text-[10px] font-semibold uppercase tracking-wide text-[var(--color-text-muted)]">
            Evidence
          </h3>
          {isCode ? (
            <pre className="mt-1 max-h-28 overflow-auto rounded border border-[var(--color-border)] bg-[var(--color-surface-alt)] p-2 text-[11px] leading-5 text-[var(--color-text)]">
              <code>{excerpt}</code>
            </pre>
          ) : (
            <p className="mt-1 line-clamp-5 text-xs leading-5 text-[var(--color-text-muted)]">
              {excerpt.slice(0, expanded ? 600 : 260)}
              {excerpt.length > (expanded ? 600 : 260) ? "..." : ""}
            </p>
          )}
        </section>

        <section className="mt-3">
          <h3 className="mb-1.5 text-[10px] font-semibold uppercase tracking-wide text-[var(--color-text-muted)]">
            Relationships
          </h3>
          <RelationshipChips data={data} fallbackKind={node.kind} />
        </section>

        <section className="mt-3">
          <h3 className="mb-1.5 text-[10px] font-semibold uppercase tracking-wide text-[var(--color-text-muted)]">
            Actions
          </h3>
          <KnowledgeActionGrid node={nodeContext} compact />
        </section>
      </div>

      {expanded && (
        <>
          <JsonEvidence nodeId={node.uid} data={data} metadata={metadata ?? null} />
          {children}
        </>
      )}

      <footer className="flex shrink-0 items-center justify-between border-t border-[var(--color-border)] px-3 py-1.5 text-[11px] text-[var(--color-text-muted)]">
        <span className="truncate" title={node.uid}>
          {node.uid}
        </span>
        <button
          type="button"
          onClick={onToggleExpanded}
          className="ml-3 shrink-0 rounded px-1.5 py-1 text-[11px] font-medium text-[var(--color-text-muted)] transition-colors hover:bg-[var(--color-surface-alt)] hover:text-[var(--color-text)]"
        >
          {expanded ? "Collapse" : "Expand"}
        </button>
      </footer>
    </article>
  );
}
