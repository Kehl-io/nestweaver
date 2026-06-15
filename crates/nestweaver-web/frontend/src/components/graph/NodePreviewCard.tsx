import { useStore } from "../../stores";
import { useNodePreview } from "../../hooks/useNodePreview";
import { KindBadge } from "../shared/KindBadge";
import { DetailPanel } from "../detail/DetailPanel";

function stripFrontmatterAndHeadings(body: string): string {
  // Strip YAML frontmatter
  let text = body.replace(/^---[\s\S]*?---\n?/, "");
  // Strip markdown headings
  text = text.replace(/^#{1,6}\s+.*/gm, "");
  // Collapse blank lines
  text = text.replace(/\n{3,}/g, "\n\n").trim();
  return text;
}

export function NodePreviewCard() {
  const previewNodeId = useStore((s) => s.previewNodeId);
  const previewExpanded = useStore((s) => s.previewExpanded);
  const closePreview = useStore((s) => s.closePreview);
  const togglePreviewExpanded = useStore((s) => s.togglePreviewExpanded);
  const selectedNodeKind = useStore((s) => s.selectedNodeKind);

  const graphInstance = useStore((s) => s.graphInstance);
  const { data, loading } = useNodePreview(previewNodeId, selectedNodeKind);

  if (!previewNodeId) return null;

  // Fallback info from graph when API detail isn't available
  const graphNode = previewNodeId && graphInstance?.hasNode(previewNodeId)
    ? {
        label: (graphInstance.getNodeAttribute(previewNodeId, "label") as string) || previewNodeId.split(":").pop() || previewNodeId,
        kind: (graphInstance.getNodeAttribute(previewNodeId, "kind") as string) || selectedNodeKind || "Unknown",
      }
    : null;

  if (previewExpanded) {
    return (
      <div
        className="absolute bottom-4 right-4 z-40 flex flex-col overflow-hidden rounded-lg border border-[var(--color-border)] bg-[var(--color-surface)] shadow-xl"
        style={{ width: 360, maxHeight: "80vh" }}
      >
        {/* Expanded header */}
        <div className="flex shrink-0 items-center justify-between border-b border-[var(--color-border)] px-3 py-2">
          <button
            onClick={togglePreviewExpanded}
            className="text-xs text-[var(--color-text-muted)] hover:text-[var(--color-text)] transition-colors"
          >
            ← Collapse
          </button>
          <button
            onClick={closePreview}
            className="text-xs text-[var(--color-text-muted)] hover:text-[var(--color-text)] transition-colors"
            aria-label="Close preview"
          >
            ✕
          </button>
        </div>
        <div className="min-h-0 flex-1 overflow-auto">
          <DetailPanel />
        </div>
      </div>
    );
  }

  return (
    <div
      className="absolute bottom-4 right-4 z-40 flex flex-col overflow-hidden rounded-lg border border-[var(--color-border)] bg-[var(--color-surface)] shadow-xl"
      style={{ maxWidth: 360, maxHeight: "50vh" }}
    >
      {loading ? (
        <div className="flex items-center justify-center p-6 text-sm text-[var(--color-text-muted)]">
          Loading...
        </div>
      ) : !data ? (
        <div className="px-3 py-3">
          <div className="flex items-start justify-between gap-2">
            <div className="flex items-center gap-1.5">
              {graphNode && <KindBadge kind={graphNode.kind} />}
              <span className="text-sm font-semibold text-[var(--color-text)]">
                {graphNode?.label ?? previewNodeId}
              </span>
            </div>
            <button
              onClick={closePreview}
              className="shrink-0 text-xs text-[var(--color-text-muted)] hover:text-[var(--color-text)] transition-colors"
              aria-label="Close preview"
            >
              ✕
            </button>
          </div>
          <p className="mt-2 text-xs text-[var(--color-text-muted)]">
            {previewNodeId}
          </p>
        </div>
      ) : (
        <>
          {/* Header */}
          <div className="flex shrink-0 items-start justify-between gap-2 border-b border-[var(--color-border)] px-3 py-2">
            <div className="flex min-w-0 flex-col gap-0.5">
              <div className="flex items-center gap-1.5">
                <KindBadge
                  kind={
                    data.type === "symbol"
                      ? data.detail.symbol.kind
                      : data.detail.note.note_kind
                  }
                />
                <span className="truncate text-sm font-semibold text-[var(--color-text)]">
                  {data.type === "symbol"
                    ? data.detail.symbol.name
                    : data.detail.note.title}
                </span>
              </div>
              <span className="truncate text-[11px] text-[var(--color-text-muted)]">
                {data.type === "symbol"
                  ? data.detail.symbol.file_path
                  : data.detail.note.file_path}
              </span>
            </div>
            <button
              onClick={closePreview}
              className="shrink-0 text-xs text-[var(--color-text-muted)] hover:text-[var(--color-text)] transition-colors"
              aria-label="Close preview"
            >
              ✕
            </button>
          </div>

          {/* Content preview */}
          <div className="min-h-0 flex-1 overflow-auto px-3 py-2">
            {data.type === "symbol" ? (
              data.sourceLines.length > 0 ? (
                <pre className="overflow-x-auto text-[11px] leading-relaxed text-[var(--color-text)]">
                  <code>{data.sourceLines.slice(0, 5).join("\n")}</code>
                </pre>
              ) : data.detail.symbol.summary ? (
                <p className="text-xs leading-5 text-[var(--color-text-muted)]">
                  {data.detail.symbol.summary}
                </p>
              ) : (
                <p className="text-xs text-[var(--color-text-muted)]">No source available</p>
              )
            ) : (
              <p className="text-xs leading-5 text-[var(--color-text-muted)]">
                {stripFrontmatterAndHeadings(data.detail.body).slice(0, 200)}
                {stripFrontmatterAndHeadings(data.detail.body).length > 200 ? "…" : ""}
              </p>
            )}
          </div>

          {/* Connections summary */}
          <div className="flex shrink-0 items-center gap-3 border-t border-[var(--color-border)] px-3 py-1.5 text-[11px] text-[var(--color-text-muted)]">
            {data.type === "symbol" ? (
              <>
                <span>
                  <span className="font-medium text-[var(--color-text)]">
                    {data.detail.callers.length}
                  </span>{" "}
                  caller{data.detail.callers.length !== 1 ? "s" : ""}
                </span>
                <span>
                  <span className="font-medium text-[var(--color-text)]">
                    {data.detail.callees.length}
                  </span>{" "}
                  callee{data.detail.callees.length !== 1 ? "s" : ""}
                </span>
              </>
            ) : (
              <>
                <span>
                  <span className="font-medium text-[var(--color-text)]">
                    {data.detail.headings.length}
                  </span>{" "}
                  heading{data.detail.headings.length !== 1 ? "s" : ""}
                </span>
                <span>
                  <span className="font-medium text-[var(--color-text)]">
                    {data.detail.note.word_count.toLocaleString()}
                  </span>{" "}
                  words
                </span>
              </>
            )}
            <button
              onClick={togglePreviewExpanded}
              className="ml-auto text-[11px] text-[var(--color-text-muted)] hover:text-[var(--color-text)] transition-colors"
            >
              Expand →
            </button>
          </div>
        </>
      )}
    </div>
  );
}
