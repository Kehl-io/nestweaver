import { useStore } from "../../stores";
import { useNodePreview } from "../../hooks/useNodePreview";
import { DetailPanel } from "../detail/DetailPanel";
import { KnowledgeCard } from "../knowledge/KnowledgeCard";

export function NodePreviewCard() {
  const previewNodeId = useStore((s) => s.previewNodeId);
  const previewExpanded = useStore((s) => s.previewExpanded);
  const closePreview = useStore((s) => s.closePreview);
  const togglePreviewExpanded = useStore((s) => s.togglePreviewExpanded);
  const selectedNodeKind = useStore((s) => s.selectedNodeKind);
  const sceneMetadata = useStore((s) => s.sceneMetadata);
  const trustSummary = useStore((s) => s.trustSummary);
  const selectedWorkspace = useStore((s) => s.selectedWorkspace());

  const graphInstance = useStore((s) => s.graphInstance);
  const { data, loading, error } = useNodePreview(previewNodeId, selectedNodeKind);

  if (!previewNodeId) return null;

  // Fallback info from graph when API detail isn't available
  const graphNode = previewNodeId && graphInstance?.hasNode(previewNodeId)
    ? {
        label: (graphInstance.getNodeAttribute(previewNodeId, "label") as string) || previewNodeId.split(":").pop() || previewNodeId,
        kind: (graphInstance.getNodeAttribute(previewNodeId, "kind") as string) || selectedNodeKind || "Unknown",
        location:
          (graphInstance.getNodeAttribute(previewNodeId, "location") as string | undefined) ??
          (graphInstance.getNodeAttribute(previewNodeId, "file_path") as string | undefined) ??
          (graphInstance.getNodeAttribute(previewNodeId, "filePath") as string | undefined) ??
          null,
      }
    : {
        label: previewNodeId.split(":").pop() || previewNodeId,
        kind: selectedNodeKind || "Unknown",
        location: null,
      };

  return (
    <div
      className="absolute bottom-4 right-4 z-40 flex flex-col overflow-hidden rounded-lg border border-[var(--color-border)] bg-[var(--color-surface)] shadow-xl"
      style={{
        width: previewExpanded ? 420 : 360,
        maxWidth: "calc(100vw - 2rem)",
        maxHeight: previewExpanded ? "84vh" : "58vh",
      }}
    >
      <KnowledgeCard
        node={{ uid: previewNodeId, ...graphNode }}
        data={data}
        loading={loading}
        error={error}
        expanded={previewExpanded}
        metadata={sceneMetadata ?? selectedWorkspace?._meta ?? null}
        trustSummary={trustSummary}
        onClose={closePreview}
        onToggleExpanded={togglePreviewExpanded}
      >
        <div className="min-h-[320px] border-t border-[var(--color-border)]">
          <DetailPanel />
        </div>
      </KnowledgeCard>
    </div>
  );
}
