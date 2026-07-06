export interface SymbolCandidate {
  uid: string;
  name: string;
  kind: string;
  file_path: string;
  start_line: number;
}

export interface Symbol {
  uid: string;
  name: string;
  kind: string;
  repo_uid: string;
  file_path: string;
  start_line: number;
  signature: string | null;
  summary: string | null;
  pagerank_score: number;
}

export interface SymbolDetail {
  symbol: Symbol;
  callers: Symbol[];
  callees: Symbol[];
}

export interface ContextNode {
  uid: string;
  name: string;
  kind: string;
  file_path: string;
  start_line: number;
  signature: string | null;
  relevance: number;
}

export interface ContextResult {
  seeds: ContextNode[];
  connected: ContextNode[];
  cross_repo_links: CrossRepoLink[];
}

export interface CrossRepoLink {
  package: string;
  link_type: string;
  confidence: number;
}

export interface BrainNode {
  uid: string;
  kind: string;
  title: string;
  location: string;
  relevance: number;
}

export interface BrainContextResult {
  seeds: BrainNode[];
  connected: BrainNode[];
  unresolved_seeds: string[];
}

export interface OverviewCounts {
  project_count: number;
  repo_count: number;
  service_count: number;
  vault_count: number;
  note_count: number;
  symbol_count: number;
  gap_count: number;
}

export interface OverviewLandmark {
  uid: string;
  kind: string;
  label: string;
  location: string;
  score: number;
  reason: string;
}

export interface OverviewGap {
  kind: string;
  label: string;
  detail: string;
}

export interface OverviewResponse {
  counts: OverviewCounts;
  landmarks: OverviewLandmark[];
  start_here: OverviewLandmark[];
  gaps: OverviewGap[];
}

export interface Repo {
  uid: string;
  url: string;
  indexed_sha: string | null;
  staleness_commits_behind: number;
  instance_id: string;
}

export interface Service {
  uid: string;
  name: string;
  repo_uid: string;
  summary: string | null;
}

export interface Vault {
  uid: string;
  name: string;
  root_path: string;
  instance_id: string;
}

export interface Note {
  uid: string;
  vault_uid: string;
  file_path: string;
  title: string;
  note_kind: string;
  word_count: number;
  pagerank_score: number;
}

export interface Heading {
  uid: string;
  note_uid: string;
  level: number;
  text: string;
  slug: string;
  start_line: number;
  end_line: number;
}

export interface Section {
  uid: string;
  note_uid: string;
  heading_uid: string | null;
  start_line: number;
  end_line: number;
  word_count: number;
  pagerank_score: number;
}

export interface Tag {
  uid: string;
  vault_uid: string;
  name: string;
}

export interface BacklinkRow {
  source_note_uid: string;
  source_note_title: string;
  source_note_path: string;
  source_section_uid: string;
  confidence: number;
  display: string | null;
}

export interface SearchHit {
  uid: string;
  kind: string;
  title: string;
  vault_uid: string;
  score: number;
}

export interface ImpactNode {
  uid: string;
  name: string;
  file_path: string;
  start_line: number;
  edge_type: string;
  confidence: number;
  depth: number;
}

export interface PathEdge {
  type: string;
  confidence: number;
}

export interface PathResult {
  nodes: string[];
  edges: PathEdge[];
  length: number;
}

export interface SourceResponse {
  file: string;
  start_line?: number;
  end_line?: number;
  lines?: string[];
  total_lines?: number;
  error?: string;
}

export interface BrainStatus {
  vault_count: number;
  note_count: number;
  heading_count: number;
  section_count: number;
  tag_count: number;
  wikilink_count: number;
  cross_domain_count: number;
}

export interface NoteDetail {
  note: Note;
  headings: Heading[];
  sections: Section[];
  body: string;
}

export interface UnlinkedMention {
  note_uid: string;
  title: string;
  path: string;
  snippet: string;
}

export interface Perspective {
  id: string;
  name: string;
  config: Record<string, unknown>;
}

export interface GapReport {
  undocumented: { module: string; symbol_count: number }[];
  untested: string[];
  disconnected_pairs: {
    community_a: string;
    community_b: string;
    similarity: number;
  }[];
}

export type GraphMode =
  | "overview"
  | "context"
  | "impact"
  | "repos"
  | "features"
  | "local";
export type ScopeFilter = "all" | "code_only" | "notes_only";
