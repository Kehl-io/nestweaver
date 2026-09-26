import { ApiError } from "./client";
import type { SymbolCandidate } from "./types";

/**
 * The repo uid every symbol in a file list shares, or `undefined` when the
 * list is empty or spans more than one repo (nw-683). With no shared repo the
 * caller omits `repo` and lets the server answer 409 for an ambiguous path.
 */
export function sharedRepoUid(symbols: SymbolCandidate[]): string | undefined {
  const first = symbols[0]?.repo_uid;
  if (!first) return undefined;
  return symbols.every((s) => s.repo_uid === first) ? first : undefined;
}

/**
 * Human text for a failed `/api/v1/source` request. Known machine codes get
 * a short explanation; any other server error shows the server's message;
 * a non-API failure (network, parse) shows `fallback`.
 */
export function sourceErrorText(error: unknown, filePath: string, fallback: string): string {
  if (!(error instanceof ApiError)) return fallback;
  switch (error.code) {
    case "ambiguous_file": {
      const n = error.candidates?.length ?? 0;
      return n > 1
        ? `Source is ambiguous: ${filePath} exists in ${n} indexed repos.`
        : `Source is ambiguous: ${filePath} exists in more than one indexed repo.`;
    }
    case "source_not_indexed":
      return `Source not available: ${filePath} is not indexed.`;
    case "source_too_large":
      return `Source not available: ${filePath} is too large to preview.`;
    default:
      return error.message || fallback;
  }
}
