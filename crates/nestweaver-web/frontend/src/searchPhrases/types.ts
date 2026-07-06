import type { SceneMetadata, WorkspaceType } from "../api/p1Types";

export type PhraseKind =
  | "explain"
  | "impact"
  | "trace_flow"
  | "callers"
  | "callees"
  | "path"
  | "tests_affected"
  | "dead_code"
  | "bridges"
  | "hubs"
  | "notes_about"
  | "backlinks"
  | "stale_repos"
  | "contract_drift";

export type PhraseTargetType =
  | "symbol"
  | "note"
  | "repo"
  | "project"
  | "workspace"
  | "file"
  | "topic"
  | "none";

export type PhraseSupportLevel = "supported" | "limited" | "unsupported" | "excluded";

export type PhraseResolutionStatus =
  | "ready"
  | "ambiguous"
  | "limited"
  | "unsupported"
  | "no-match"
  | "error";

export interface PhraseIntent {
  kind: PhraseKind;
  input: string;
  normalized: string;
  rawTarget?: string;
  rawSource?: string;
  rawDestination?: string;
  targetTypes: PhraseTargetType[];
}

export interface PhraseCoverageEntry {
  kind: PhraseKind;
  phrase: string;
  supportLevel: PhraseSupportLevel;
  previewRequired: boolean;
  behavior: string;
  limits: string[];
}

export interface PhraseCandidate {
  id: string;
  uid?: string;
  label: string;
  kind: string;
  targetType: PhraseTargetType;
  detail?: string;
  score?: number;
  workspaceType?: WorkspaceType | string;
}

export type PhraseTargetRole = "target" | "source" | "destination";

export interface PhraseResolvedTarget {
  id: string;
  uid?: string;
  label: string;
  kind: string;
  targetType: PhraseTargetType;
  detail?: string;
  role?: PhraseTargetRole;
}

export interface PhraseCandidateGroup {
  role: PhraseTargetRole;
  label: string;
  candidates: PhraseCandidate[];
}

export type PhraseCandidateOverrides = Partial<
  Record<PhraseTargetRole, PhraseCandidate>
>;

export interface PhraseResolution {
  intent: PhraseIntent;
  status: PhraseResolutionStatus;
  supportLevel: PhraseSupportLevel;
  previewRequired: boolean;
  title: string;
  summary: string;
  actionLabel?: string;
  targets: PhraseResolvedTarget[];
  candidateGroups: PhraseCandidateGroup[];
  metadata: SceneMetadata | null;
  coverage: PhraseCoverageEntry;
}

export interface PhraseExecutionResult {
  status: "executed" | "limited" | "unsupported" | "error";
  message: string;
}
