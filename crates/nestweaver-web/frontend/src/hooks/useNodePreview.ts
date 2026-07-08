import { useEffect, useRef, useState } from "react";
import type { NoteDetail, SourceResponse, SymbolDetail } from "../api/types";

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

async function fetchJson<T>(url: string, signal: AbortSignal): Promise<T> {
  const response = await fetch(url, { signal });
  if (!response.ok) {
    const body = await response.json().catch(() => ({ error: response.statusText }));
    throw new Error(body.error || response.statusText);
  }
  return response.json() as Promise<T>;
}

function symbolUrl(uid: string): string {
  return `/api/v1/symbol/${encodeURIComponent(uid)}`;
}

function noteUrl(uid: string): string {
  return `/api/v1/brain/note/${encodeURIComponent(uid)}`;
}

function sourceUrl(file: string, line?: number, context?: number): string {
  let url = `/api/v1/source?file=${encodeURIComponent(file)}`;
  if (line != null) url += `&line=${line}`;
  if (context != null) url += `&context=${context}`;
  return url;
}

export function useNodePreview(
  nodeId: string | null,
  nodeKind: string | null,
): { data: PreviewData; loading: boolean; error: string | null } {
  const [data, setData] = useState<PreviewData>(null);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const requestSeqRef = useRef(0);

  useEffect(() => {
    const requestSeq = requestSeqRef.current + 1;
    requestSeqRef.current = requestSeq;

    if (!nodeId) {
      setData(null);
      setLoading(false);
      setError(null);
      return;
    }

    const cached = cache.get(nodeId);
    if (cached) {
      setData(cached);
      setLoading(false);
      setError(null);
      return;
    }

    const controller = new AbortController();
    const isCurrent = () =>
      requestSeqRef.current === requestSeq && !controller.signal.aborted;

    setData(null);
    setLoading(true);
    setError(null);

    const isNote =
      nodeId.startsWith("note:") ||
      nodeKind === "note" ||
      nodeKind === "Note";

    // Repos and services have no symbol detail; treat "no preview" as an
    // expected empty state, not an error (repo hubs are the landing scene)
    const isContainer =
      nodeId.startsWith("repo:") ||
      nodeId.startsWith("svc:") ||
      nodeKind === "repo" ||
      nodeKind === "service";
    if (isContainer) {
      setData(null);
      setLoading(false);
      setError(null);
      return () => controller.abort();
    }

    const fetchData = async () => {
      try {
        if (isNote) {
          const detail = await fetchJson<NoteDetail>(noteUrl(nodeId), controller.signal);
          const result: PreviewData = { type: "note", detail };
          cacheSet(nodeId, result);
          if (isCurrent()) setData(result);
        } else {
          const detail = await fetchJson<SymbolDetail>(symbolUrl(nodeId), controller.signal);
          let sourceLines: string[] = [];
          try {
            const source = await fetchJson<SourceResponse>(
              sourceUrl(detail.symbol.file_path, detail.symbol.start_line, 5),
              controller.signal,
            );
            sourceLines = source.lines ?? [];
          } catch (sourceError) {
            if (controller.signal.aborted) throw sourceError;
            // Source snippets can be unavailable while symbol metadata is still useful.
          }
          const result: PreviewData = { type: "symbol", detail, sourceLines };
          cacheSet(nodeId, result);
          if (isCurrent()) setData(result);
        }
      } catch (fetchError) {
        if (isCurrent()) {
          setData(null);
          setError(
            fetchError instanceof Error && fetchError.message
              ? fetchError.message
              : "Failed to load preview",
          );
        }
      } finally {
        if (isCurrent()) setLoading(false);
      }
    };

    fetchData();
    return () => controller.abort();
  }, [nodeId, nodeKind]);

  return { data, loading, error };
}
