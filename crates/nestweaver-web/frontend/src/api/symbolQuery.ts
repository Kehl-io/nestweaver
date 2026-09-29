import { api } from "./client";
import { ApiError } from "./errors";
import type { SymbolDetail } from "./types";

/**
 * One shared symbol-detail query per uid (nw-567).
 *
 * Details, Evidence and the breadcrumb all describe the same selection; each
 * used to fetch `/api/v1/symbol/{uid}` on its own, so a missing node cost
 * three 404s and each pane rendered its own error. Callers now share one
 * in-flight request and its settled result. A 404 is remembered (the node is
 * not in the index) until the next graph update; other failures are
 * forgotten so the next caller retries.
 */
const MAX_ENTRIES = 16;
const entries = new Map<string, Promise<SymbolDetail>>();
// Bumped whenever remembered results are dropped, so views re-query the
// node they show (a missing node may exist after a re-index).
let generation = 0;
const listeners = new Set<() => void>();

export function subscribeSymbolQueries(listener: () => void): () => void {
  listeners.add(listener);
  return () => listeners.delete(listener);
}

export function symbolQueryGeneration(): number {
  return generation;
}

export function isNotFoundError(error: unknown): boolean {
  return error instanceof ApiError && error.status === 404;
}

export function fetchSymbol(uid: string): Promise<SymbolDetail> {
  const existing = entries.get(uid);
  if (existing) {
    // Refresh recency.
    entries.delete(uid);
    entries.set(uid, existing);
    return existing;
  }
  const request = api.symbol(uid).catch((error: unknown) => {
    if (!isNotFoundError(error) && entries.get(uid) === request) {
      entries.delete(uid);
    }
    throw error;
  });
  entries.set(uid, request);
  while (entries.size > MAX_ENTRIES) {
    const oldest = entries.keys().next().value;
    if (oldest === undefined) break;
    entries.delete(oldest);
  }
  return request;
}

/** Forget every settled result, e.g. after the graph was re-indexed. */
export function clearSymbolQueries(): void {
  entries.clear();
  generation += 1;
  listeners.forEach((listener) => listener());
}
