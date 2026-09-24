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
  NotesPage,
  UnlinkedMention,
  Vault,
} from "./types";
import { loadImpactLens } from "./impactLens";
import { appendWorkspaceParam } from "./workspaces";
import { normalizeBrainContext } from "./context";

/** Page size the notes route allows (`LIST_NOTES_LIMIT_MAX`). */
export const NOTES_PAGE_SIZE = 1000;
/**
 * The notes route serves no offset past `LIST_NOTES_LIMIT_MAX`, so a vault's
 * first `NOTES_MAX_REACHABLE` notes are listable; the rest must be disclosed.
 */
export const NOTES_MAX_REACHABLE = 2 * NOTES_PAGE_SIZE;

export class ApiError extends Error {
  status: number;

  constructor(status: number, message: string) {
    super(message);
    this.status = status;
  }
}

async function request<T>(url: string, init?: RequestInit): Promise<T> {
  const res = await fetch(url, init);
  if (init?.signal?.aborted) {
    throw new DOMException("Aborted", "AbortError");
  }
  if (!res.ok) {
    const body = await res.json().catch(() => ({ error: res.statusText }));
    throw new ApiError(res.status, body.error || res.statusText);
  }
  return res.json() as Promise<T>;
}

function get<T>(url: string, init?: RequestInit): Promise<T> {
  return request<T>(url, init);
}

function post<T>(url: string, body: unknown, signal?: AbortSignal): Promise<T> {
  return request<T>(url, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(body),
    signal,
  });
}

export const api = {
  search(q: string, limit = 20, init?: RequestInit) {
    return get<SymbolCandidate[]>(
      `/api/v1/search?q=${encodeURIComponent(q)}&limit=${limit}`,
      init,
    );
  },

  symbol(uid: string, init?: RequestInit) {
    return get<SymbolDetail>(
      `/api/v1/symbol/${encodeURIComponent(uid)}`,
      init,
    );
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
    tokenBudget: number | null = 4096,
    scope: ScopeFilter = "all",
    workspaceId?: string | null,
    signal?: AbortSignal,
  ) {
    return post<unknown>("/api/v1/brain/context", {
      seeds,
      token_budget: tokenBudget ?? undefined,
      scope,
      workspace: workspaceId ?? undefined,
    }, signal).then(normalizeBrainContext);
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

  // NotesTab is a catalog, not a "top N" view, so it requests the API maximum
  // page (1000) PER VAULT (nw-648: one unfiltered page held only the first
  // vault). The handler still caps omitted `limit` at 20 so curl/MCP cannot
  // dump the whole vault; `total` comes from `X-Total-Count`.
  async brainNotesPage(vaultUid: string, offset = 0, limit = NOTES_PAGE_SIZE): Promise<NotesPage> {
    const params = new URLSearchParams({
      vault: vaultUid,
      limit: String(limit),
      offset: String(offset),
    });
    const res = await fetch(`/api/v1/brain/notes?${params}`);
    if (!res.ok) {
      const body = await res.json().catch(() => ({ error: res.statusText }));
      throw new ApiError(res.status, body.error || res.statusText);
    }
    const header = res.headers.get("x-total-count");
    const total = header === null ? null : Number.parseInt(header, 10);
    return {
      notes: (await res.json()) as Note[],
      total: total === null || Number.isNaN(total) ? null : total,
    };
  },

  brainNote(uid: string, init?: RequestInit) {
    return get<NoteDetail>(
      `/api/v1/brain/note/${encodeURIComponent(uid)}`,
      init,
    );
  },

  // Envelope `{ backlinks, count, total, truncated, limit }` since the HTTP
  // handler gained a notes-list-style cap. Unwrap so UI callers still receive
  // `BacklinkRow[]`. Default 1000 matches `brainNotesPage` (the UI needs the full
  // page; omitted `limit` on the API is 20).
  async brainBacklinks(uid: string, limit = 1000, init?: RequestInit) {
    const payload = await get<BacklinkRow[] | { backlinks?: BacklinkRow[] }>(
      `/api/v1/brain/backlinks/${encodeURIComponent(uid)}?limit=${limit}`,
      init,
    );
    if (Array.isArray(payload)) {
      return payload;
    }
    return payload.backlinks ?? [];
  },

  brainUnlinkedMentions(uid: string, init?: RequestInit) {
    return get<UnlinkedMention[]>(
      `/api/v1/brain/unlinked-mentions/${encodeURIComponent(uid)}`,
      init,
    );
  },

  brainSearch(q: string, limit = 20, init?: RequestInit) {
    return get<SearchHit[]>(
      `/api/v1/brain/search?q=${encodeURIComponent(q)}&limit=${limit}`,
      init,
    );
  },

  source(file: string, line?: number, context?: number, init?: RequestInit) {
    let url = `/api/v1/source?file=${encodeURIComponent(file)}`;
    if (line != null) url += `&line=${line}`;
    if (context != null) url += `&context=${context}`;
    return get<SourceResponse>(url, init);
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
