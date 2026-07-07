import type { SceneMetadata } from "./p1Types";

export interface ImpactSourceEvidence {
  file_path: string;
  start_line: number;
  url: string;
}

export interface ImpactLensNode {
  uid: string;
  name: string;
  file_path: string;
  start_line: number;
  layer: number;
  role: "target" | "impact" | string;
  confidence: number;
  impact_score: number;
  edge_type?: string | null;
  source: ImpactSourceEvidence;
}

export interface ImpactLensEdge {
  source: string;
  target: string;
  edge_type: string;
  confidence: number;
  source_layer: number;
  target_layer: number;
}

export interface AffectedTestFile {
  test_file: string;
  tests: string[];
  symbol_uid: string;
  confidence: number;
}

export interface AffectedTestsResult {
  changed_files: string[];
  changed_symbols: {
    uid: string;
    name: string;
    file_path: string;
  }[];
  tier_1: AffectedTestFile[];
  tier_2: AffectedTestFile[];
  tier_3: AffectedTestFile[];
  summary: string;
  disclaimer: string;
}

export interface ImpactLensStates {
  tier: "two-tier" | "local-only" | string;
  local: "available" | "unavailable" | string;
  org: "available" | "unavailable" | string;
  freshness: "current" | "stale" | "unknown" | string;
  timeout: "not-timed-out" | "timed-out" | string;
  permission: "not-requested" | "permission-rejected" | string;
  read_only: "not-read-only" | "read-only" | string;
  result: string;
}

export interface ImpactLensResponse {
  target: ImpactLensNode;
  nodes: ImpactLensNode[];
  edges: ImpactLensEdge[];
  affected_tests: AffectedTestsResult;
  states: ImpactLensStates;
  _meta: SceneMetadata;
}

export interface ImpactLensOptions {
  depth?: number;
  confidence?: number;
  workspaceId?: string | null;
  limit?: number;
}

async function request<T>(url: string): Promise<T> {
  const response = await fetch(url);
  if (!response.ok) {
    const body = await response.json().catch(() => ({ error: response.statusText }));
    throw new Error(body.error || response.statusText);
  }
  return response.json() as Promise<T>;
}

export function impactLensUrl(uid: string, options: ImpactLensOptions = {}): string {
  const params = new URLSearchParams();
  params.set("depth", String(options.depth ?? 3));
  params.set("confidence", String(options.confidence ?? 0.3));
  params.set("limit", String(options.limit ?? 250));
  if (options.workspaceId) params.set("workspace", options.workspaceId);
  return `/api/v1/impact/${encodeURIComponent(uid)}?${params.toString()}`;
}

export function loadImpactLens(
  uid: string,
  options: ImpactLensOptions = {},
): Promise<ImpactLensResponse> {
  return request<ImpactLensResponse>(impactLensUrl(uid, options));
}
