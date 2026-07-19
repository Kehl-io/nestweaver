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
  tier: "two-tier" | "local-only" | "unsupported" | string;
  local: "available" | "unavailable" | "unsupported" | string;
  org: "available" | "org-unavailable" | "unknown" | "not-applicable" | string;
  freshness: "current" | "stale" | "unknown" | "not-applicable" | string;
  timeout: "unknown" | "timed-out" | "not-applicable" | string;
  permission: "unknown" | "permission-rejected" | "not-applicable" | string;
  read_only: "unknown" | "read-only" | "not-applicable" | string;
  result: string;
}

export interface ImpactLensResponse {
  target: ImpactLensNode | null;
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

const IMPACT_TIMEOUT_MS = 30_000;

/** Thrown when an impact request exceeds IMPACT_TIMEOUT_MS. */
export class ImpactTimeoutError extends Error {
  constructor() {
    super("Impact request timed out");
    this.name = "ImpactTimeoutError";
  }
}

async function request<T>(url: string, signal?: AbortSignal): Promise<T> {
  const timeout = AbortSignal.timeout(IMPACT_TIMEOUT_MS);
  const combined = signal ? AbortSignal.any([signal, timeout]) : timeout;
  let response: Response;
  try {
    response = await fetch(url, { signal: combined });
  } catch (err) {
    // Distinguish our own timeout from a caller-initiated abort: the timeout
    // signal firing means the backend was still computing (e.g. cold-DB
    // PageRank) when the budget elapsed.
    if (timeout.aborted) throw new ImpactTimeoutError();
    throw err;
  }
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
  signal?: AbortSignal,
): Promise<ImpactLensResponse> {
  return request<ImpactLensResponse>(impactLensUrl(uid, options), signal);
}
