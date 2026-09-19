import type {
  BacklinkRow,
  BrainContextResult,
  BrainStatus,
  ContextResult,
  FlowNode,
  GapReport,
  Note,
  NoteDetail,
  OverviewResponse,
  PathResult,
  Perspective,
  Repo,
  ScopeFilter,
  SearchHit,
  Service,
  SourceResponse,
  SymbolCandidate,
  SymbolDetail,
  Tag,
  UnlinkedMention,
  Vault,
} from "./types";
import { loadImpactLens } from "./impactLens";
import { appendWorkspaceParam } from "./workspaces";

export class ApiError extends Error {
  status: number;

  constructor(status: number, message: string) {
    super(message);
    this.status = status;
  }
}

async function request<T>(url: string, init?: RequestInit): Promise<T> {
  const res = await fetch(url, init);
  if (!res.ok) {
    const body = await res.json().catch(() => ({ error: res.statusText }));
    throw new ApiError(res.status, body.error || res.statusText);
  }
  return res.json() as Promise<T>;
}

function get<T>(url: string): Promise<T> {
  return request<T>(url);
}

function post<T>(url: string, body: unknown): Promise<T> {
  return request<T>(url, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(body),
  });
}

export const api = {
  search(q: string, limit = 20) {
    return get<SymbolCandidate[]>(
      `/api/v1/search?q=${encodeURIComponent(q)}&limit=${limit}`,
    );
  },

  symbol(uid: string) {
    return get<SymbolDetail>(`/api/v1/symbol/${encodeURIComponent(uid)}`);
  },

  symbolsInFile(path: string) {
    return get<SymbolCandidate[]>(
      `/api/v1/symbols/file?path=${encodeURIComponent(path)}`,
    );
  },

  symbolsTop(limit = 20, workspaceId?: string | null) {
    const url = appendWorkspaceParam(`/api/v1/symbols/top?limit=${limit}`, workspaceId);
    return get<SymbolCandidate[]>(url);
  },

  context(seeds: string[], limit = 30) {
    return post<ContextResult>("/api/v1/context", { seeds, limit });
  },

  brainContext(
    seeds: string[],
    tokenBudget = 4096,
    scope: ScopeFilter = "all",
  ) {
    return post<BrainContextResult>("/api/v1/brain/context", {
      seeds,
      token_budget: tokenBudget,
      scope,
    });
  },

  overview(limit = 24) {
    return get<OverviewResponse>(`/api/v1/overview?limit=${limit}`);
  },

  impact(uid: string, depth = 3, confidence = 0.3, workspaceId?: string | null) {
    return loadImpactLens(uid, { depth, confidence, workspaceId });
  },

  repos() {
    return get<Repo[]>("/api/v1/repos");
  },

  services() {
    return get<Service[]>("/api/v1/services");
  },

  repoMap(budget = 2000) {
    return get<string>(`/api/v1/repo-map?budget=${budget}`);
  },

  suggestLinks() {
    return get<CrossRepoLinkSuggestion[]>("/api/v1/suggest-links");
  },

  brainStatus() {
    return get<BrainStatus>("/api/v1/brain/status");
  },

  brainVaults() {
    return get<Vault[]>("/api/v1/brain/vaults");
  },

  brainTags() {
    return get<Tag[]>("/api/v1/brain/tags");
  },

  // NotesTab is a catalog, not a "top N" view, so request the API maximum
  // (1000). Pagination is not wired; the handler still caps omitted `limit`
  // at 20 so curl/MCP cannot dump the whole vault.
  brainNotes(limit = 1000) {
    return get<Note[]>(`/api/v1/brain/notes?limit=${limit}`);
  },

  brainNote(uid: string) {
    return get<NoteDetail>(`/api/v1/brain/note/${encodeURIComponent(uid)}`);
  },

  // Envelope `{ backlinks, count, total, truncated, limit }` since the HTTP
  // handler gained a notes-list-style cap. Unwrap so UI callers still receive
  // `BacklinkRow[]`. Default 1000 matches `brainNotes` (the UI needs the full
  // page; omitted `limit` on the API is 20).
  async brainBacklinks(uid: string, limit = 1000) {
    const payload = await get<BacklinkRow[] | { backlinks?: BacklinkRow[] }>(
      `/api/v1/brain/backlinks/${encodeURIComponent(uid)}?limit=${limit}`,
    );
    if (Array.isArray(payload)) {
      return payload;
    }
    return payload.backlinks ?? [];
  },

  brainUnlinkedMentions(uid: string) {
    return get<UnlinkedMention[]>(
      `/api/v1/brain/unlinked-mentions/${encodeURIComponent(uid)}`,
    );
  },

  brainSearch(q: string, limit = 20) {
    return get<SearchHit[]>(
      `/api/v1/brain/search?q=${encodeURIComponent(q)}&limit=${limit}`,
    );
  },

  source(file: string, line?: number, context?: number) {
    let url = `/api/v1/source?file=${encodeURIComponent(file)}`;
    if (line != null) url += `&line=${line}`;
    if (context != null) url += `&context=${context}`;
    return get<SourceResponse>(url);
  },

  paths(from: string, to: string, maxDepth = 5, limit = 10) {
    return get<PathResult[]>(
      `/api/v1/paths/${encodeURIComponent(from)}/${encodeURIComponent(to)}?max_depth=${maxDepth}&limit=${limit}`,
    );
  },

  flow(uid: string, maxDepth = 5) {
    return get<FlowNode>(
      `/api/v1/flow/${encodeURIComponent(uid)}?max_depth=${maxDepth}`,
    );
  },

  gaps() {
    return get<GapReport>("/api/v1/gaps");
  },

  perspectives() {
    return get<Perspective[]>("/api/v1/perspectives");
  },

  createPerspective(name: string, config: Record<string, unknown>) {
    return post<Perspective>("/api/v1/perspectives", { name, config });
  },

  llmQuery(query: string, tokenBudget = 4096) {
    return post<{ seeds: string[]; explanation: string; context: BrainContextResult }>("/api/v1/llm/query", {
      query,
      token_budget: tokenBudget,
    });
  },
};

export async function loadGapItems(): Promise<import("../stores/analysisSlice").GapItem[]> {
  const report = await api.gaps();
  return [
    ...report.undocumented.map((m) => ({
      type: "undocumented" as const,
      label: m.module,
      detail: `${m.symbol_count} symbols with no documentation`,
      nodeUids: [] as string[],
    })),
    ...report.untested.map((uid) => ({
      type: "untested" as const,
      label: uid.split(":").pop() || uid,
      detail: "Entry point with no test coverage",
      nodeUids: [uid],
    })),
  ];
}

/** Return type for suggest-links; not in shared types since it's endpoint-specific. */
interface CrossRepoLinkSuggestion {
  source: string;
  target: string;
  confidence: number;
  reason: string;
}
