import { useEffect, useRef, useState } from "react";
import { ApiError, apiErrorFromBody } from "../api/errors";
import { isFileSelection, isNoteSelection } from "../api/kinds";
import { fetchSymbol, isNotFoundError } from "../api/symbolQuery";
import { useStore } from "../stores";
import { useSymbolQueryGeneration } from "./useSymbolQuery";
import type {
  NoteDetail,
  SourceResponse,
  SymbolCandidate,
  SymbolDetail,
} from "../api/types";

export type PreviewData =
  | { type: "symbol"; detail: SymbolDetail; sourceLines: string[] }
  | { type: "note"; detail: NoteDetail }
  | { type: "file"; path: string; symbols: SymbolCandidate[]; sourceLines: string[] }
  | null;

const cache = new Map<string, PreviewData>();
const CACHE_MAX = 10;

export function clearNodePreviews(): void { cache.clear(); }

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
    throw apiErrorFromBody(response.status, body, response.statusText);
  }
  return response.json() as Promise<T>;
}

function noteUrl(uid: string): string {
  return `/api/v1/brain/note/${encodeURIComponent(uid)}`;
}

function symbolsInFileUrl(path: string): string {
  return `/api/v1/symbols/file?path=${encodeURIComponent(path)}`;
}

function sourceUrl(file: string, line?: number, context?: number, repo?: string): string {
  let url = `/api/v1/source?file=${encodeURIComponent(file)}`;
  // nw-683: name the repo so a path indexed by several repos is not a 409.
  if (repo) url += `&repo=${encodeURIComponent(repo)}`;
  if (line != null) url += `&line=${line}`;
  if (context != null) url += `&context=${context}`;
  return url;
}

export function useNodePreview(
  nodeId: string | null,
  nodeKind: string | null,
): { data: PreviewData; loading: boolean; error: string | null; notFound: boolean } {
  const [data, setData] = useState<PreviewData>(null);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [notFound, setNotFound] = useState(false);
  const requestSeqRef = useRef(0);
  const graphEpoch = useStore((s) => s.graphEpoch);
  const workspaceId = useStore((s) => s.activeWorkspaceId);
  const symbolGeneration = useSymbolQueryGeneration();

  useEffect(() => {
    const requestSeq = requestSeqRef.current + 1;
    requestSeqRef.current = requestSeq;

    setNotFound(false);
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
      requestSeqRef.current === requestSeq && !controller.signal.aborted && useStore.getState().graphEpoch === graphEpoch && useStore.getState().activeWorkspaceId === workspaceId;

    setData(null);
    setLoading(true);
    setError(null);

    const isNote = isNoteSelection(nodeId, nodeKind);
    const isFile = isFileSelection(nodeId, nodeKind);

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
          if (isCurrent()) { cacheSet(nodeId, result); setData(result); }
        } else if (isFile) {
          let symbols: SymbolCandidate[] = [];
          try {
            symbols = await fetchJson<SymbolCandidate[]>(
              symbolsInFileUrl(nodeId),
              controller.signal,
            );
          } catch (symbolsError) {
            if (controller.signal.aborted) throw symbolsError;
          }
          let sourceLines: string[] = [];
          try {
            const source = await fetchJson<SourceResponse>(
              sourceUrl(nodeId, symbols[0]?.start_line ?? 1, 12),
              controller.signal,
            );
            sourceLines = source.lines ?? [];
          } catch (sourceError) {
            if (controller.signal.aborted) throw sourceError;
            // An ambiguous file needs a deliberate repo choice in the detail
            // view; showing one repo's symbols as its source would mislead.
            if (sourceError instanceof ApiError && sourceError.code === "ambiguous_file") {
              throw sourceError;
            }
          }
          if (symbols.length === 0 && sourceLines.length === 0) {
            throw new Error("File evidence is unavailable.");
          }
          const result: PreviewData = { type: "file", path: nodeId, symbols, sourceLines };
          if (isCurrent()) { cacheSet(nodeId, result); setData(result); }
        } else {
          // Shared with Details/Evidence so one selection costs one request.
          const detail = await fetchSymbol(nodeId);
          if (controller.signal.aborted) return;
          let sourceLines: string[] = [];
          try {
            const source = await fetchJson<SourceResponse>(
              sourceUrl(
                detail.symbol.file_path,
                detail.symbol.start_line,
                5,
                detail.symbol.repo_uid,
              ),
              controller.signal,
            );
            sourceLines = source.lines ?? [];
          } catch (sourceError) {
            if (controller.signal.aborted) throw sourceError;
            // Source snippets can be unavailable while symbol metadata is still useful.
          }
          const result: PreviewData = { type: "symbol", detail, sourceLines };
          if (isCurrent()) { cacheSet(nodeId, result); setData(result); }
        }
      } catch (fetchError) {
        if (isCurrent()) {
          setData(null);
          setNotFound(isNotFoundError(fetchError));
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
  }, [nodeId, nodeKind, symbolGeneration, graphEpoch, workspaceId]);

  return { data, loading, error, notFound };
}
