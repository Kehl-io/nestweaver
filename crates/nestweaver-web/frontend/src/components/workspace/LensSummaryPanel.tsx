import { AlertTriangle, CheckCircle2, Info, Network } from "lucide-react";
import type { SceneMetadata, TrustSummary } from "../../api/p1Types";
import { useStore } from "../../stores";
import { WorkspaceScopeSummary } from "./WorkspaceScopeSummary";

interface LensSummaryPanelProps {
  compact?: boolean;
  className?: string;
}

function tone(summary: TrustSummary | null): string {
  if (!summary) return "border-[var(--color-border)] text-[var(--color-text-muted)]";
  if (summary.result === "error" || summary.result === "timed-out") {
    return "border-red-500/35 bg-red-500/10 text-red-200";
  }
  if (summary.result === "unsupported" || summary.unsupported.length > 0) {
    return "border-amber-500/35 bg-amber-500/10 text-amber-200";
  }
  if (summary.partial || summary.result === "partial" || summary.result === "truncated") {
    return "border-sky-500/35 bg-sky-500/10 text-sky-200";
  }
  return "border-emerald-500/35 bg-emerald-500/10 text-emerald-200";
}

function SummaryIcon({ summary }: { summary: TrustSummary | null }) {
  if (!summary) return <Info className="h-4 w-4" />;
  if (summary.result === "error" || summary.result === "timed-out") {
    return <AlertTriangle className="h-4 w-4" />;
  }
  if (summary.result === "unsupported" || summary.unsupported.length > 0) {
    return <Info className="h-4 w-4" />;
  }
  return <CheckCircle2 className="h-4 w-4" />;
}

function metadataFacts(metadata: SceneMetadata | null): string[] {
  if (!metadata) return ["No scene metadata yet"];
  const facts = [
    metadata.trust.data_scope,
    metadata.trust.federation,
    metadata.trust.freshness,
    metadata.trust.source_confidence,
  ].filter(Boolean);
  if (metadata.truncation.truncated) facts.push("truncated");
  if (metadata.continuation.has_more) facts.push("continuation available");
  return Array.from(new Set(facts));
}

export function LensSummaryPanel({
  compact = false,
  className = "",
}: LensSummaryPanelProps) {
  const activeLens = useStore((s) => s.activeLens);
  const sceneMetadata = useStore((s) => s.sceneMetadata);
  const trustSummary = useStore((s) => s.trustSummary);
  const graphInstance = useStore((s) => s.graphInstance);
  const gapItems = useStore((s) => s.gapItems);
  const flowTraceNodeUids = useStore((s) => s.flowTraceNodeUids);
  const pathResults = useStore((s) => s.pathResults);
  const selectedNodeId = useStore((s) => s.selectedNodeId);
  const facts = metadataFacts(sceneMetadata);
  const nodeCount = graphInstance?.order ?? 0;
  const edgeCount = graphInstance?.size ?? 0;

  return (
    <section
      aria-label="Lens summary"
      className={`rounded border p-3 text-xs ${tone(trustSummary)} ${className}`}
    >
      <div className="flex min-w-0 items-start gap-2">
        <span className="mt-0.5 shrink-0">
          <SummaryIcon summary={trustSummary} />
        </span>
        <div className="min-w-0 flex-1">
          <p className="truncate font-semibold text-[var(--color-text)]">
            {activeLens.label}
          </p>
          <p className="mt-1 leading-5 text-[var(--color-text-muted)]">
            {trustSummary?.message ||
              sceneMetadata?.trust.message ||
              "Scene metadata will appear as results load."}
          </p>
        </div>
      </div>

      {!compact && (
        <>
          <div className="mt-3">
            <WorkspaceScopeSummary metadata={sceneMetadata} compact />
          </div>
          <dl className="mt-3 grid grid-cols-2 gap-2 text-[11px]">
            <div className="rounded border border-[var(--color-border)] bg-[var(--color-surface)]/55 px-2 py-1.5">
              <dt className="text-[var(--color-text-muted)]">Graph</dt>
              <dd className="mt-0.5 font-medium text-[var(--color-text)]">
                {nodeCount} nodes · {edgeCount} edges
              </dd>
            </div>
            <div className="rounded border border-[var(--color-border)] bg-[var(--color-surface)]/55 px-2 py-1.5">
              <dt className="text-[var(--color-text-muted)]">Analysis</dt>
              <dd className="mt-0.5 font-medium text-[var(--color-text)]">
                {flowTraceNodeUids.length || pathResults.length || gapItems.length || 0} items
              </dd>
            </div>
          </dl>
          <div className="mt-3 flex flex-wrap gap-1.5">
            {facts.map((fact) => (
              <span
                key={fact}
                className="inline-flex items-center gap-1 rounded border border-[var(--color-border)] bg-[var(--color-surface)]/60 px-1.5 py-0.5 text-[10px] text-[var(--color-text-muted)]"
              >
                <Network className="h-3 w-3" />
                {fact}
              </span>
            ))}
          </div>
          {trustSummary?.unsupported && trustSummary.unsupported.length > 0 && (
            <p className="mt-3 text-[11px] leading-5 text-amber-300">
              Unavailable: {trustSummary.unsupported.join(", ")}
            </p>
          )}
          {selectedNodeId && (
            <p className="mt-3 break-all border-t border-[var(--color-border)] pt-2 text-[11px] text-[var(--color-text-muted)]">
              Selected: {selectedNodeId}
            </p>
          )}
        </>
      )}
    </section>
  );
}
