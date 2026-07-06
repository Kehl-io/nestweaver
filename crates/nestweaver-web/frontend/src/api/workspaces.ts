import { ApiError } from "./client";
import type { SearchHit } from "./types";
import type {
  ScopedBrainSearchResponse,
  WorkspaceCatalogResponse,
} from "./p1Types";

async function request<T>(url: string, init?: RequestInit): Promise<T> {
  const res = await fetch(url, init);
  if (!res.ok) {
    const body = await res.json().catch(() => ({ error: res.statusText }));
    throw new ApiError(res.status, body.error || res.statusText);
  }
  return res.json() as Promise<T>;
}

export interface WorkspaceScopedOptions {
  workspaceId?: string | null;
  limit?: number;
}

export function workspaceQueryParams(
  workspaceId?: string | null,
): URLSearchParams {
  const params = new URLSearchParams();
  if (workspaceId) params.set("workspace", workspaceId);
  return params;
}

export function appendWorkspaceParam(
  url: string,
  workspaceId?: string | null,
): string {
  if (!workspaceId) return url;
  const separator = url.includes("?") ? "&" : "?";
  return `${url}${separator}workspace=${encodeURIComponent(workspaceId)}`;
}

export function workspaceContextBody(
  seeds: string[],
  tokenBudget = 4096,
  workspaceId?: string | null,
): { seeds: string[]; token_budget: number; workspace?: string } {
  const body: { seeds: string[]; token_budget: number; workspace?: string } = {
    seeds,
    token_budget: tokenBudget,
  };
  if (workspaceId) body.workspace = workspaceId;
  return body;
}

export function loadWorkspaces(): Promise<WorkspaceCatalogResponse> {
  return request<WorkspaceCatalogResponse>("/api/v1/workspaces");
}

export function brainSearchInWorkspace(
  q: string,
  options: WorkspaceScopedOptions = {},
): Promise<ScopedBrainSearchResponse<SearchHit>> {
  const params = workspaceQueryParams(options.workspaceId ?? "all");
  params.set("q", q);
  params.set("limit", String(options.limit ?? 20));
  return request<ScopedBrainSearchResponse<SearchHit>>(
    `/api/v1/brain/search?${params.toString()}`,
  );
}
