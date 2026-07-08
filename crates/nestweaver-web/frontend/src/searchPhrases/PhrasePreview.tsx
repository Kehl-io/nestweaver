import { useEffect, useMemo, useState } from "react";
import type { PhraseCandidate, PhraseResolution } from "./types";
import type { PhraseCandidateOverrides, PhraseTargetRole } from "./types";

interface PhrasePreviewProps {
  resolution: PhraseResolution | null;
  resolving: boolean;
  onExecute: (candidateOverrides?: PhraseCandidateOverrides) => void;
  onCandidateExecute: (candidate: PhraseCandidate) => void;
}

function supportLabel(resolution: PhraseResolution): string {
  if (resolution.supportLevel === "limited") return "Limited";
  if (resolution.supportLevel === "unsupported") return "Unsupported";
  if (resolution.status === "ambiguous") return "Ambiguous";
  if (resolution.status === "no-match") return "No match";
  return "Phrase";
}

export function PhrasePreview({
  resolution,
  resolving,
  onExecute,
  onCandidateExecute,
}: PhrasePreviewProps) {
  const [pathSelections, setPathSelections] = useState<PhraseCandidateOverrides>({});
  const selectionKey = useMemo(() => {
    if (!resolution) return "";
    return [
      resolution.intent.normalized,
      ...resolution.candidateGroups.map((group) =>
        `${group.role}:${group.candidates.map((candidate) => candidate.id).join(",")}`,
      ),
    ].join("|");
  }, [resolution]);

  useEffect(() => {
    setPathSelections({});
  }, [selectionKey]);

  if (resolving) {
    return (
      <div className="border-b border-[var(--color-border)] px-3 py-2 text-sm text-[var(--color-text-muted)]">
        Resolving phrase...
      </div>
    );
  }
  if (!resolution) return null;
  const currentResolution = resolution;

  function roleIsResolved(role: PhraseTargetRole): boolean {
    return (
      pathSelections[role] !== undefined ||
      currentResolution.targets.some((target) => target.role === role)
    );
  }

  const pathCanRun =
    resolution.intent.kind === "path" &&
    roleIsResolved("source") &&
    roleIsResolved("destination");
  const canRun =
    pathCanRun ||
    resolution.status === "ready" ||
    resolution.status === "limited" ||
    resolution.status === "unsupported";
  const meta = resolution.metadata;

  return (
    <div className="border-b border-[var(--color-border)] px-3 py-2">
      <div className="mb-1 flex items-start justify-between gap-2">
        <div className="min-w-0">
          <div className="flex items-center gap-2">
            <span className="text-sm font-medium">{resolution.title}</span>
            <span className="rounded border border-[var(--color-border)] px-1.5 py-0.5 text-[10px] uppercase tracking-wide text-[var(--color-text-muted)]">
              {supportLabel(resolution)}
            </span>
          </div>
          <p className="mt-1 text-xs text-[var(--color-text-muted)]">
            {resolution.summary}
          </p>
        </div>
        {canRun && (
          <button
            type="button"
            onClick={() =>
              onExecute(resolution.intent.kind === "path" ? pathSelections : undefined)
            }
            disabled={resolution.status === "unsupported"}
            className="shrink-0 rounded border border-[var(--color-border)] px-2 py-1 text-xs text-[var(--color-graph-selection)] hover:bg-[var(--color-surface-alt)] disabled:cursor-not-allowed disabled:opacity-50"
          >
            {resolution.actionLabel ?? "Run"}
          </button>
        )}
      </div>

      {resolution.candidateGroups.map((group) => (
        <div key={`${group.role}:${group.label}`} className="mt-2">
          <div className="mb-1 text-[10px] font-semibold uppercase tracking-wide text-[var(--color-text-muted)]">
            Choose {group.role}: {group.label}
          </div>
          <div className="space-y-1">
            {group.candidates.length === 0 ? (
              <div className="text-xs text-[var(--color-text-muted)]">
                No candidates found.
              </div>
            ) : (
              group.candidates.map((candidate) => (
                <button
                  key={`${group.role}:${candidate.targetType}:${candidate.id}`}
                  type="button"
                  onClick={() => {
                    if (resolution.intent.kind === "path") {
                      setPathSelections((current) => ({
                        ...current,
                        [group.role]: candidate,
                      }));
                      return;
                    }
                    onCandidateExecute(candidate);
                  }}
                  aria-pressed={
                    resolution.intent.kind === "path" &&
                    pathSelections[group.role]?.id === candidate.id
                  }
                  className="flex w-full items-center justify-between gap-2 rounded px-2 py-1 text-left text-xs hover:bg-[var(--color-surface-alt)] aria-pressed:bg-[var(--color-surface-alt)] aria-pressed:text-[var(--color-graph-selection)]"
                >
                  <span className="min-w-0">
                    <span className="font-medium">{candidate.label}</span>
                    {candidate.detail && (
                      <span className="ml-2 text-[var(--color-text-muted)]">
                        {candidate.detail}
                      </span>
                    )}
                  </span>
                  <span className="shrink-0 text-[10px] uppercase text-[var(--color-text-muted)]">
                    {resolution.intent.kind === "path" &&
                    pathSelections[group.role]?.id === candidate.id
                      ? "selected"
                      : candidate.kind || candidate.targetType}
                  </span>
                </button>
              ))
            )}
          </div>
          {resolution.intent.kind === "path" && group.candidates.length > 0 && (
            <p className="mt-1 text-[10px] text-[var(--color-text-muted)]">
              Select candidates for both endpoints, then run the path search.
            </p>
          )}
        </div>
      ))}

      <div className="mt-2 flex flex-wrap gap-1 text-[10px] text-[var(--color-text-muted)]">
        <span className="rounded bg-[var(--color-surface-alt)] px-1.5 py-0.5">
          {resolution.coverage.supportLevel}
        </span>
        {resolution.previewRequired && (
          <span className="rounded bg-[var(--color-surface-alt)] px-1.5 py-0.5">
            preview
          </span>
        )}
        {meta?.trust.freshness && (
          <span className="rounded bg-[var(--color-surface-alt)] px-1.5 py-0.5">
            {meta.trust.freshness}
          </span>
        )}
        {meta?.truncation.truncated && (
          <span className="rounded bg-[var(--color-surface-alt)] px-1.5 py-0.5">
            truncated
          </span>
        )}
        {meta?.continuation.has_more && (
          <span className="rounded bg-[var(--color-surface-alt)] px-1.5 py-0.5">
            continuation
          </span>
        )}
      </div>
    </div>
  );
}
