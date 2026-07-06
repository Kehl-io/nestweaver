import { api } from "../api/client";
import { isSymbolKind } from "../api/kinds";
import type { SceneMetadata, ScopedSearchHit, WorkspaceEntry } from "../api/p1Types";
import type { SearchHit, SymbolCandidate } from "../api/types";
import { brainSearchInWorkspace } from "../api/workspaces";
import { phraseCoverage } from "./phraseCoverage";
import type {
  PhraseCandidate,
  PhraseCandidateGroup,
  PhraseTargetRole,
  PhraseIntent,
  PhraseResolution,
  PhraseResolutionStatus,
  PhraseResolvedTarget,
  PhraseSupportLevel,
  PhraseTargetType,
} from "./types";

export interface ResolvePhraseOptions {
  activeWorkspaceId: string;
  workspaces: WorkspaceEntry[];
  symbolResults?: SymbolCandidate[];
  brainResults?: SearchHit[];
}

interface SearchPool {
  symbols: SymbolCandidate[];
  brain: SearchHit[];
}

function defaultMetadata(
  workspace: WorkspaceEntry | null,
  result: SceneMetadata["trust"]["result"],
  message: string,
  unsupported: string[] = [],
): SceneMetadata | null {
  const base = workspace?._meta;
  if (!base) return null;
  return {
    ...base,
    trust: {
      ...base.trust,
      result,
      partial: result !== "complete" || base.trust.partial,
      unsupported: [...base.trust.unsupported, ...unsupported],
      message,
    },
  };
}

function selectedWorkspace(options: ResolvePhraseOptions): WorkspaceEntry | null {
  return (
    options.workspaces.find((workspace) => workspace.id === options.activeWorkspaceId) ??
    options.workspaces.find((workspace) => workspace.id === "all") ??
    null
  );
}

function isNoteLike(kind: string): boolean {
  return ["note", "Note", "heading", "section", "tag"].includes(kind);
}

function splitScopedSearchResults(results: ScopedSearchHit[]): SearchPool {
  return results.reduce(
    (acc, hit) => {
      if ("repo_uid" in hit && "file_path" in hit) {
        acc.symbols.push({
          uid: hit.uid,
          name: hit.name || hit.title,
          kind: "symbol",
          file_path: hit.file_path,
          start_line: 0,
        });
      } else {
        acc.brain.push({
          uid: hit.uid,
          kind: hit.kind,
          title: hit.title,
          vault_uid: hit.vault_uid,
          score: hit.score,
        });
      }
      return acc;
    },
    { symbols: [] as SymbolCandidate[], brain: [] as SearchHit[] },
  );
}

async function searchTargets(query: string, options: ResolvePhraseOptions): Promise<SearchPool> {
  if (options.activeWorkspaceId === "all") {
    const [symbols, brain] = await Promise.all([
      api.search(query, 8).catch(() => [] as SymbolCandidate[]),
      api.brainSearch(query, 8).catch(() => [] as SearchHit[]),
    ]);
    return { symbols, brain };
  }

  const scoped = await brainSearchInWorkspace(query, {
    workspaceId: options.activeWorkspaceId,
    limit: 12,
  });
  return splitScopedSearchResults(scoped.results);
}

function symbolCandidate(symbol: SymbolCandidate): PhraseCandidate {
  return {
    id: symbol.uid,
    uid: symbol.uid,
    label: symbol.name,
    kind: symbol.kind,
    targetType: "symbol",
    detail: symbol.file_path,
  };
}

function noteCandidate(hit: SearchHit): PhraseCandidate {
  return {
    id: hit.uid,
    uid: hit.uid,
    label: hit.title,
    kind: hit.kind,
    targetType: "note",
    detail: hit.vault_uid,
    score: hit.score,
  };
}

function workspaceCandidate(workspace: WorkspaceEntry): PhraseCandidate {
  const targetType: PhraseTargetType =
    workspace.type === "repo" || workspace.type === "project"
      ? workspace.type
      : "workspace";
  return {
    id: workspace.id,
    uid: workspace.uid,
    label: workspace.label,
    kind: workspace.type,
    targetType,
    detail: workspace.uid ?? workspace.id,
    workspaceType: workspace.type,
    metadata: workspace._meta,
  };
}

function candidateToTarget(
  candidate: PhraseCandidate,
  role?: PhraseTargetRole,
): PhraseResolvedTarget {
  return {
    id: candidate.id,
    uid: candidate.uid,
    label: candidate.label,
    kind: candidate.kind,
    targetType: candidate.targetType,
    detail: candidate.detail,
    role,
    workspaceType: candidate.workspaceType,
    metadata: candidate.metadata,
  };
}

function normalized(value: string): string {
  return value.trim().toLowerCase();
}

function bestCandidates(raw: string, candidates: PhraseCandidate[]): PhraseCandidate[] {
  const query = normalized(raw);
  const exact = candidates.filter((candidate) => normalized(candidate.label) === query);
  if (exact.length > 0) return exact;
  return candidates;
}

function uniqueCandidates(candidates: PhraseCandidate[]): PhraseCandidate[] {
  const seen = new Set<string>();
  return candidates.filter((candidate) => {
    const key = `${candidate.targetType}:${candidate.id}`;
    if (seen.has(key)) return false;
    seen.add(key);
    return true;
  });
}

async function resolveCandidates(
  raw: string,
  targetTypes: PhraseTargetType[],
  options: ResolvePhraseOptions,
): Promise<PhraseCandidate[]> {
  const all: PhraseCandidate[] = [];

  if (targetTypes.some((type) => type === "repo" || type === "project" || type === "workspace")) {
    const query = normalized(raw);
    all.push(
      ...options.workspaces
        .filter((workspace) => {
          const typeMatches =
            targetTypes.includes("workspace") ||
            targetTypes.includes(workspace.type as PhraseTargetType);
          if (!typeMatches) return false;
          return (
            normalized(workspace.label).includes(query) ||
            normalized(workspace.id).includes(query) ||
            normalized(workspace.uid ?? "").includes(query)
          );
        })
        .map(workspaceCandidate),
    );
  }

  if (targetTypes.includes("file")) {
    const matchingFiles = (options.symbolResults ?? [])
      .filter((symbol) => normalized(symbol.file_path).includes(normalized(raw)))
      .map((symbol) => ({
        id: symbol.file_path,
        label: symbol.file_path,
        kind: "file",
        targetType: "file" as const,
        detail: symbol.name,
      }));
    all.push(...matchingFiles);
  }

  if (targetTypes.includes("symbol") || targetTypes.includes("note")) {
    const pool = await searchTargets(raw, options);
    if (targetTypes.includes("symbol")) {
      all.push(...pool.symbols.filter((symbol) => isSymbolKind(symbol.kind)).map(symbolCandidate));
    }
    if (targetTypes.includes("note")) {
      all.push(...pool.brain.filter((hit) => isNoteLike(hit.kind)).map(noteCandidate));
    }
  }

  return uniqueCandidates(bestCandidates(raw, all)).slice(0, 8);
}

function resolution(
  intent: PhraseIntent,
  status: PhraseResolutionStatus,
  supportLevel: PhraseSupportLevel,
  title: string,
  summary: string,
  options: ResolvePhraseOptions,
  fields: Partial<PhraseResolution> = {},
): PhraseResolution {
  const coverage = phraseCoverage[intent.kind];
  const unsupported =
    supportLevel === "unsupported"
      ? [coverage.behavior]
      : supportLevel === "limited"
        ? coverage.limits
        : [];
  return {
    intent,
    status,
    supportLevel,
    previewRequired: coverage.previewRequired,
    title,
    summary,
    actionLabel: coverage.previewRequired ? "Run" : "Open",
    targets: [],
    candidateGroups: [],
    metadata: defaultMetadata(
      selectedWorkspace(options),
      status === "unsupported"
        ? "unsupported"
        : status === "ambiguous"
          ? "ambiguous"
          : status === "no-match"
            ? "no-match"
            : supportLevel === "limited"
              ? "partial"
              : "complete",
      summary,
      unsupported,
    ),
    coverage,
    ...fields,
  };
}

function noTargetResolution(intent: PhraseIntent, options: ResolvePhraseOptions): PhraseResolution {
  const support = phraseCoverage[intent.kind].supportLevel;
  if (intent.kind === "contract_drift") {
    return resolution(
      intent,
      "unsupported",
      support,
      "Contract drift",
      "Contract drift is not wired to a P1 web route yet.",
      options,
      { actionLabel: "Unavailable" },
    );
  }
  return resolution(
    intent,
    support === "limited" ? "limited" : "ready",
    support,
    intent.kind === "stale_repos" ? "Stale repos" : phraseCoverage[intent.kind].phrase,
    phraseCoverage[intent.kind].behavior,
    options,
  );
}

function candidateGroup(
  role: PhraseTargetRole,
  label: string,
  candidates: PhraseCandidate[],
): PhraseCandidateGroup {
  return { role, label, candidates };
}

async function resolveOne(
  intent: PhraseIntent,
  raw: string | undefined,
  targetTypes: PhraseTargetType[],
  options: ResolvePhraseOptions,
  role: PhraseTargetRole = "target",
): Promise<PhraseResolution> {
  if (!raw) {
    return resolution(
      intent,
      "no-match",
      phraseCoverage[intent.kind].supportLevel,
      phraseCoverage[intent.kind].phrase,
      "Enter a target after the phrase.",
      options,
    );
  }

  const candidates = await resolveCandidates(raw, targetTypes, options);
  if (candidates.length === 0) {
    return resolution(
      intent,
      "no-match",
      phraseCoverage[intent.kind].supportLevel,
      phraseCoverage[intent.kind].phrase,
      `No ${targetTypes.join(" or ")} target matched "${raw}".`,
      options,
    );
  }

  if (candidates.length > 1) {
    return resolution(
      intent,
      "ambiguous",
      phraseCoverage[intent.kind].supportLevel,
      phraseCoverage[intent.kind].phrase,
      `Choose which target matches "${raw}".`,
      options,
      { candidateGroups: [candidateGroup(role, raw, candidates)] },
    );
  }

  const target = candidateToTarget(candidates[0], role);
  const support = phraseCoverage[intent.kind].supportLevel;
  return resolution(
    intent,
    support === "limited" ? "limited" : "ready",
    support,
    phraseCoverage[intent.kind].phrase,
    `${phraseCoverage[intent.kind].behavior} Target: ${target.label}.`,
    options,
    { targets: [target] },
  );
}

async function resolvePath(
  intent: PhraseIntent,
  options: ResolvePhraseOptions,
): Promise<PhraseResolution> {
  const sourceRaw = intent.rawSource;
  const destinationRaw = intent.rawDestination;
  if (!sourceRaw || !destinationRaw) {
    return resolution(
      intent,
      "no-match",
      phraseCoverage.path.supportLevel,
      phraseCoverage.path.phrase,
      "Enter both path endpoints.",
      options,
    );
  }

  const [sourceCandidates, destinationCandidates] = await Promise.all([
    resolveCandidates(sourceRaw, ["symbol"], options),
    resolveCandidates(destinationRaw, ["symbol"], options),
  ]);
  const groups: PhraseCandidateGroup[] = [];
  const resolvedTargets: PhraseResolvedTarget[] = [];
  if (sourceCandidates.length !== 1) {
    groups.push(candidateGroup("source", sourceRaw, sourceCandidates));
  } else {
    resolvedTargets.push(candidateToTarget(sourceCandidates[0], "source"));
  }
  if (destinationCandidates.length !== 1) {
    groups.push(candidateGroup("destination", destinationRaw, destinationCandidates));
  } else {
    resolvedTargets.push(candidateToTarget(destinationCandidates[0], "destination"));
  }
  if (groups.length > 0) {
    return resolution(
      intent,
      sourceCandidates.length === 0 || destinationCandidates.length === 0
        ? "no-match"
        : "ambiguous",
      phraseCoverage.path.supportLevel,
      phraseCoverage.path.phrase,
      "Resolve both endpoints before running path search.",
      options,
      { candidateGroups: groups, targets: resolvedTargets },
    );
  }

  const targets = [
    candidateToTarget(sourceCandidates[0], "source"),
    candidateToTarget(destinationCandidates[0], "destination"),
  ];
  return resolution(
    intent,
    "ready",
    phraseCoverage.path.supportLevel,
    phraseCoverage.path.phrase,
    `Path preview from ${targets[0].label} to ${targets[1].label}.`,
    options,
    { targets },
  );
}

async function resolveNotesAbout(
  intent: PhraseIntent,
  options: ResolvePhraseOptions,
): Promise<PhraseResolution> {
  const raw = intent.rawTarget;
  if (!raw) {
    return resolution(
      intent,
      "no-match",
      phraseCoverage.notes_about.supportLevel,
      phraseCoverage.notes_about.phrase,
      "Enter a note topic.",
      options,
    );
  }
  const candidates = await resolveCandidates(raw, ["note"], options);
  return resolution(
    intent,
    "ready",
    phraseCoverage.notes_about.supportLevel,
    phraseCoverage.notes_about.phrase,
    candidates.length > 0
      ? `Found note candidates for "${raw}".`
      : `No note candidates matched "${raw}" yet; open rationale search metadata instead.`,
    options,
    {
      candidateGroups:
        candidates.length > 0 ? [candidateGroup("target", raw, candidates)] : [],
    },
  );
}

export async function resolveSearchPhrase(
  intent: PhraseIntent,
  options: ResolvePhraseOptions,
): Promise<PhraseResolution> {
  try {
    if (intent.kind === "stale_repos" || intent.kind === "contract_drift") {
      return noTargetResolution(intent, options);
    }
    if (intent.kind === "path") {
      return resolvePath(intent, options);
    }
    if (intent.kind === "notes_about") {
      return resolveNotesAbout(intent, options);
    }
    if (intent.kind === "tests_affected") {
      return resolveOne(intent, intent.rawTarget, ["symbol", "file"], options);
    }
    return resolveOne(intent, intent.rawTarget, intent.targetTypes, options);
  } catch (error) {
    const message =
      error instanceof Error && error.message
        ? error.message
        : "Phrase resolution failed";
    return resolution(
      intent,
      "error",
      phraseCoverage[intent.kind].supportLevel,
      phraseCoverage[intent.kind].phrase,
      message,
      options,
    );
  }
}

export function phraseCandidateToTarget(candidate: PhraseCandidate): PhraseResolvedTarget {
  return candidateToTarget(candidate);
}
