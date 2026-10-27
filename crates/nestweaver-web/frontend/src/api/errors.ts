export class ApiError extends Error {
  status: number;
  /** Machine code from the body's `error` field (e.g. `ambiguous_file`). */
  code?: string;
  /** Repo uids an ambiguous `/source` path matched (409 `ambiguous_file`). */
  candidates?: string[];
  /** Qualified indexed owner on typed `/source` body-unavailable errors. */
  repo?: string;
  file?: string;

  constructor(status: number, message: string, code?: string, candidates?: string[], repo?: string, file?: string) {
    super(message);
    this.status = status;
    this.code = code;
    this.candidates = candidates;
    this.repo = repo;
    this.file = file;
  }
}

/** Build an `ApiError` from a non-2xx JSON body. */
export function apiErrorFromBody(status: number, body: unknown, statusText: string): ApiError {
  const b = (body && typeof body === "object" ? body : {}) as Record<string, unknown>;
  const code = typeof b.error === "string" && b.error ? b.error : undefined;
  const message =
    (typeof b.message === "string" && b.message) || code || statusText;
  const candidates = Array.isArray(b.candidates)
    ? b.candidates.filter((c): c is string => typeof c === "string")
    : undefined;
  const repo = typeof b.repo === "string" && b.repo ? b.repo : undefined;
  const file = typeof b.file === "string" && b.file ? b.file : undefined;
  return new ApiError(status, message, code, candidates, repo, file);
}
