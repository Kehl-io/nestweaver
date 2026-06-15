import { useEffect, useState } from "react";
import { api } from "../api/client";
import type { SymbolDetail, NoteDetail } from "../api/types";

export type PreviewData =
  | { type: "symbol"; detail: SymbolDetail; sourceLines: string[] }
  | { type: "note"; detail: NoteDetail }
  | null;

const cache = new Map<string, PreviewData>();
const CACHE_MAX = 10;

function cacheSet(key: string, value: PreviewData) {
  if (cache.size >= CACHE_MAX) {
    const oldest = cache.keys().next().value;
    if (oldest !== undefined) cache.delete(oldest);
  }
  cache.set(key, value);
}

export function useNodePreview(
  nodeId: string | null,
  nodeKind: string | null,
): { data: PreviewData; loading: boolean } {
  const [data, setData] = useState<PreviewData>(null);
  const [loading, setLoading] = useState(false);

  useEffect(() => {
    if (!nodeId) {
      setData(null);
      return;
    }

    const cached = cache.get(nodeId);
    if (cached) {
      setData(cached);
      return;
    }

    const controller = new AbortController();
    setLoading(true);

    const isNote =
      nodeId.startsWith("note:") ||
      nodeKind === "note" ||
      nodeKind === "Note";

    const fetchData = async () => {
      try {
        if (isNote) {
          const detail = await api.brainNote(nodeId);
          const result: PreviewData = { type: "note", detail };
          cacheSet(nodeId, result);
          if (!controller.signal.aborted) setData(result);
        } else {
          const detail = await api.symbol(nodeId);
          let sourceLines: string[] = [];
          try {
            const source = await api.source(
              detail.symbol.file_path,
              detail.symbol.start_line,
              5,
            );
            sourceLines = source.lines ?? [];
          } catch {
            // source not available — still show the card
          }
          const result: PreviewData = { type: "symbol", detail, sourceLines };
          cacheSet(nodeId, result);
          if (!controller.signal.aborted) setData(result);
        }
      } catch {
        if (!controller.signal.aborted) setData(null);
      } finally {
        if (!controller.signal.aborted) setLoading(false);
      }
    };

    fetchData();
    return () => controller.abort();
  }, [nodeId, nodeKind]);

  return { data, loading };
}
