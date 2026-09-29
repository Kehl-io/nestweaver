import { useEffect, useState } from "react";
import { fetchSymbol, isNotFoundError } from "../api/symbolQuery";
import type { SymbolDetail } from "../api/types";

export type SymbolQueryStatus = "idle" | "loading" | "found" | "missing" | "error";

export interface SymbolQuery {
  status: SymbolQueryStatus;
  detail: SymbolDetail | null;
  error: string | null;
}

const IDLE: SymbolQuery = { status: "idle", detail: null, error: null };
const LOADING: SymbolQuery = { status: "loading", detail: null, error: null };

/** Symbol detail for `uid` through the shared query; `null` means no symbol. */
export function useSymbolQuery(uid: string | null): SymbolQuery {
  const [result, setResult] = useState<{ uid: string | null; query: SymbolQuery }>({
    uid: null,
    query: IDLE,
  });

  useEffect(() => {
    if (!uid) return;
    let cancelled = false;
    fetchSymbol(uid)
      .then((detail) => {
        if (!cancelled) setResult({ uid, query: { status: "found", detail, error: null } });
      })
      .catch((error: unknown) => {
        if (cancelled) return;
        setResult({
          uid,
          query: isNotFoundError(error)
            ? { status: "missing", detail: null, error: null }
            : {
                status: "error",
                detail: null,
                error: error instanceof Error && error.message
                  ? error.message
                  : "Symbol detail is unavailable.",
              },
        });
      });
    return () => {
      cancelled = true;
    };
  }, [uid]);

  if (!uid) return IDLE;
  // A result for a previous uid is never shown for the current one.
  return result.uid === uid ? result.query : LOADING;
}
