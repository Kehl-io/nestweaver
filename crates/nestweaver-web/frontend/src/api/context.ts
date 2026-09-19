import type { BrainContextResult, BrainNode } from "./types";

function nodes(value: unknown, field: string): BrainNode[] {
  if (!Array.isArray(value) || value.some((node) => !node || typeof node !== "object" ||
    typeof node.uid !== "string" || typeof node.kind !== "string" ||
    typeof node.title !== "string" || typeof node.location !== "string" ||
    typeof node.relevance !== "number" || !Number.isFinite(node.relevance))) {
    throw new Error(`Context response has invalid ${field}; comparison data is unavailable.`);
  }
  return value as BrainNode[];
}

/** The server omits unresolved_seeds when empty. Required populations must
 * remain errors when absent; they are not interchangeable with empty results. */
export function normalizeBrainContext(value: unknown): BrainContextResult {
  if (!value || typeof value !== "object") throw new Error("Context response is unavailable.");
  const payload = value as Record<string, unknown>;
  const unresolved = payload.unresolved_seeds === undefined ? [] : payload.unresolved_seeds;
  if (!Array.isArray(unresolved) || unresolved.some((seed) => typeof seed !== "string")) {
    throw new Error("Context response has invalid unresolved seeds.");
  }
  return {
    ...payload,
    seeds: nodes(payload.seeds, "seeds"),
    connected: nodes(payload.connected, "connected nodes"),
    unresolved_seeds: unresolved,
  } as BrainContextResult;
}
