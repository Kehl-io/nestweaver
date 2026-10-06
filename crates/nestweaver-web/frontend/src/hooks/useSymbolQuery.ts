import { useEffect, useState, useSyncExternalStore } from "react";
import {
  fetchSymbol,
  isNotFoundError,
  subscribeSymbolQueries,
  symbolQueryGeneration,
} from "../api/symbolQuery";
import { useStore } from "../stores";
import type { SymbolDetail } from "../api/types";

export type SymbolQueryStatus = "idle" | "loading" | "found" | "missing" | "error";

export interface SymbolQuery {
  status: SymbolQueryStatus;
  detail: SymbolDetail | null;
  error: string | null;
}

const IDLE: SymbolQuery = { status: "idle", detail: null, error: null };
const LOADING: SymbolQuery = { status: "loading", detail: null, error: null };

/** Changes whenever the shared symbol cache is dropped (after a graph update). */
export function useSymbolQueryGeneration(): number {
  return useSyncExternalStore(subscribeSymbolQueries, symbolQueryGeneration);
}

/** Symbol detail for `uid` through the shared query; `null` means no symbol. */
export function useSymbolQuery(uid: string | null): SymbolQuery {
  const generation = useSymbolQueryGeneration();
  const epoch = useStore((s) => s.graphEpoch);
  const workspaceId = useStore((s) => s.activeWorkspaceId);
  const [result, setResult] = useState<{ uid: string | null; query: SymbolQuery }>({
    uid: null,
    query: IDLE,
  });

  useEffect(() => {
    if (!uid) return;
    let cancelled = false;
    const current = () => !cancelled && useStore.getState().graphEpoch === epoch && useStore.getState().activeWorkspaceId === workspaceId;
    fetchSymbol(uid)
      .then((detail) => {
        if (current()) setResult({ uid, query: { status: "found", detail, error: null } });
      })
      .catch((error: unknown) => {
        if (!current()) return;
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
    // A new generation re-queries the same uid; the last result stays shown
    // until the fresh one lands.
  }, [uid, generation, epoch, workspaceId]);

  if (!uid) return IDLE;
  // A result for a previous uid is never shown for the current one.
  return result.uid === uid ? result.query : LOADING;
}
