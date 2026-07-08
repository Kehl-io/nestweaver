export type WorkspaceType = "all" | "project" | "repo" | "vault";

export interface WorkspaceCounts {
  project_count: number;
  repo_count: number;
  service_count: number;
  vault_count: number;
  note_count: number;
  symbol_count: number;
}

export type TrustDataScope =
  | "all"
  | "local-only"
  | "federated"
  | "repo-scoped"
  | "project-scoped"
  | "vault-scoped";

export type UpstreamState =
  | "healthy"
  | "unreachable"
  | "ejected"
  | "backoff"
  | "timed-out"
  | "permission-rejected"
  | "unknown";

export type FreshnessState =
  | "current"
  | "stale"
  | "indexing"
  | "unknown"
  | "partial";

export type CapabilityState =
  | "local-index"
  | "read-only-replica"
  | "admin-enabled"
  | "daemon-down-direct-mode"
  | "unknown";

export type ResultState =
  | "loading"
  | "complete"
  | "partial"
  | "unsupported"
  | "empty"
  | "no-match"
  | "ambiguous"
  | "truncated"
  | "timed-out"
  | "cancelled"
  | "error";

export type SourceConfidence =
  | "extracted"
  | "inferred"
  | "heuristic"
  | "unresolved"
  | "unknown";

export interface TrustMetadata {
  data_scope: TrustDataScope | string;
  federation: "local-only" | "federated" | "unknown" | string;
  freshness: FreshnessState | string;
  capability: CapabilityState | string;
  result: ResultState | string;
  source_confidence: SourceConfidence | string;
  partial: boolean;
  unsupported: string[];
  message: string;
}

export interface ProvenanceMetadata {
  source: string;
  detail: string;
}

export interface TruncationMetadata {
  truncated: boolean;
  limit?: number | null;
  omitted_count?: number | null;
}

export interface ContinuationMetadata {
  has_more: boolean;
  cursor?: string | null;
  reason?: string | null;
}

export interface SceneMetadata {
  workspace_id: string;
  workspace_type: WorkspaceType | string;
  trust: TrustMetadata;
  provenance: ProvenanceMetadata[];
  truncation: TruncationMetadata;
  continuation: ContinuationMetadata;
}

export interface WorkspaceEntry {
  id: string;
  type: WorkspaceType;
  label: string;
  uid?: string;
  counts: WorkspaceCounts;
  _meta: SceneMetadata;
}

export interface WorkspaceCatalogResponse {
  workspaces: WorkspaceEntry[];
  _meta: SceneMetadata;
}

export type ActiveLens =
  | "overview"
  | "context"
  | "search"
  | "impact"
  | "trace"
  | "path"
  | "rationale"
  | "freshness"
  | "unsupported";

export interface ActiveLensState {
  lens: ActiveLens;
  label: string;
  targetUid?: string | null;
  workspaceId?: string | null;
}

export type RepresentationMode = "graph" | "list" | "table" | "matrix" | "json";

export interface TrustSummary {
  dataScope: TrustDataScope | string;
  freshness: FreshnessState | string;
  federation: string;
  result: ResultState | string;
  partial: boolean;
  unsupported: string[];
  message?: string;
}

export interface ScopedBrainSearchResponse<T> {
  results: T[];
  _meta: SceneMetadata;
}

export type ScopedNoteSearchKind =
  | "Note"
  | "note"
  | "heading"
  | "section"
  | "tag";

export interface ScopedNoteSearchHit {
  uid: string;
  kind: ScopedNoteSearchKind;
  title: string;
  vault_uid: string;
  score: number;
}

export interface ScopedSymbolSearchHit {
  uid: string;
  kind: "Symbol";
  name: string;
  title: string;
  repo_uid: string;
  file_path: string;
  score: number;
}

export type ScopedSearchHit = ScopedNoteSearchHit | ScopedSymbolSearchHit;
