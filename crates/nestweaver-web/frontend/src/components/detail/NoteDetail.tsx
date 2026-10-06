import { useCallback, useEffect, useRef, useState } from "react";
import { api } from "../../api/client";
import type {
  BacklinkRow,
  NoteDetail as NoteDetailType,
  UnlinkedMention,
} from "../../api/types";
import { useStore } from "../../stores";
import { NodeActionBar } from "../actions/NodeActionBar";
import { Collapsible } from "../shared/Collapsible";
import { KindBadge } from "../shared/KindBadge";
import { MarkdownPreview } from "./MarkdownPreview";

interface NoteDetailProps {
  uid: string;
}

export function NoteDetail({ uid }: NoteDetailProps) {
  const graphEpoch = useStore((s) => s.graphEpoch);
  const workspaceId = useStore((s) => s.activeWorkspaceId);
  const bodyRef = useRef<HTMLDivElement>(null);
  const navigationSeq = useRef(0);
  const loadedNote = useRef<string | null>(null);
  const errorNote = useRef<string | null>(null);
  const destination = useStore((s) => s.noteHeadingDestination);
  const setDestination = useStore((s) => s.setNoteHeadingDestination);
  const [linkError, setLinkError] = useState<string | null>(null);
  const exploreNode = useStore((s) => s.exploreNode);
  const detailFocus = useStore((s) => s.detailFocus);

  const [detail, setDetail] = useState<NoteDetailType | null>(null);
  const [backlinks, setBacklinks] = useState<BacklinkRow[]>([]);
  const [unlinked, setUnlinked] = useState<UnlinkedMention[]>([]);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    const controller = new AbortController();
    setLinkError(null);
    navigationSeq.current += 1;
    const noteKey = JSON.stringify([workspaceId, uid]);
    const revalidating = loadedNote.current === noteKey;
    if (!revalidating) { loadedNote.current = null; setDetail(null); setBacklinks([]); setUnlinked([]); }
    setLoading(!revalidating);
    setError(null);

    const init = { signal: controller.signal };
    Promise.all([
      api.brainNote(uid, init),
      api.brainBacklinks(uid, 1000, init).catch(() => [] as BacklinkRow[]),
      api.brainUnlinkedMentions(uid, init).catch(() => [] as UnlinkedMention[]),
    ])
      .then(([note, bl, um]) => {
        if (!controller.signal.aborted && useStore.getState().graphEpoch === graphEpoch && useStore.getState().activeWorkspaceId === workspaceId) {
          loadedNote.current = noteKey;
          setDetail(note);
          setBacklinks(bl);
          setUnlinked(um);
        }
      })
      .catch((e) => {
        if (!controller.signal.aborted && useStore.getState().graphEpoch === graphEpoch && useStore.getState().activeWorkspaceId === workspaceId) { errorNote.current = noteKey; setError(e.message ?? "Failed to load note"); }
      })
      .finally(() => {
        if (!controller.signal.aborted && useStore.getState().graphEpoch === graphEpoch && useStore.getState().activeWorkspaceId === workspaceId) setLoading(false);
      });
    return () => { navigationSeq.current += 1; controller.abort(); };
  }, [uid, graphEpoch, workspaceId]);

  useEffect(() => {
    if (loading || !destination?.heading || destination.uid !== uid) return;
    const heading = bodyRef.current?.querySelector<HTMLElement>(`[data-heading-uid="${CSS.escape(destination.heading)}"]`);
    if (heading) { heading.focus(); heading.scrollIntoView({ block: "nearest" }); setDestination(null); }
  }, [loading, destination, uid, detail, setDestination]);

  const handleWikilink = useCallback(async (target: string) => {
    const sequence = ++navigationSeq.current;
    setLinkError(null);
    try {
      const resolved = await api.resolveWikilink(uid, target);
      if (sequence !== navigationSeq.current || useStore.getState().graphEpoch !== graphEpoch || useStore.getState().activeWorkspaceId !== workspaceId || useStore.getState().selectedNodeId !== uid) return;
      exploreNode(resolved.note_uid, "note");
      setDestination({ uid: resolved.note_uid, heading: resolved.heading_uid });
    } catch (error) {
      if (sequence !== navigationSeq.current || useStore.getState().graphEpoch !== graphEpoch || useStore.getState().activeWorkspaceId !== workspaceId || useStore.getState().selectedNodeId !== uid) return;
      setLinkError(error instanceof Error ? error.message : "Wikilink could not be resolved.");
    }
  }, [uid, graphEpoch, workspaceId, exploreNode, setDestination]);

  const visibleNoteKey = JSON.stringify([workspaceId, uid]);
  if (loading || (loadedNote.current !== visibleNoteKey && !(error && errorNote.current === visibleNoteKey))) {
    return (
      <div className="flex h-full items-center justify-center text-sm text-[var(--color-text-muted)]">
        Loading note...
      </div>
    );
  }

  if (error && errorNote.current === visibleNoteKey) {
    return (
      <div className="flex h-full items-center justify-center p-4 text-sm text-red-500">
        {error}
      </div>
    );
  }

  if (!detail) {
    return (
      <div className="flex h-full items-center justify-center text-sm text-[var(--color-text-muted)]">
        Note not found.
      </div>
    );
  }

  const { note, headings, body } = detail;

  return (
    <div className="flex min-w-0 h-full flex-col overflow-x-hidden overflow-y-auto p-4">
      {/* Top: Identity */}
      <div className="mb-4">
        <div className="mb-1 flex min-w-0 items-start gap-2">
          <KindBadge kind="Note" />
          <span className="min-w-0 break-all text-sm font-semibold text-[var(--color-text)]">
            {note.title}
          </span>
        </div>
        <div className="mb-2 min-w-0 break-all text-xs text-[var(--color-text-muted)]">
          {note.file_path}
        </div>
        <div className="flex gap-4 text-xs text-[var(--color-text-muted)]">
          <span>{note.word_count} words</span>
          <span>{headings.length} headings</span>
          <span>PageRank: {note.pagerank_score.toFixed(4)}</span>
        </div>
        <NodeActionBar
          node={{ uid: note.uid, kind: "note", label: note.title }}
          ids={["open", "explore", "related", "path", "ask", "copyLink"]}
          compact
          className="mt-3"
        />
      </div>

      {/* Middle: Outline & References */}
      <div className="mb-4 space-y-1">
        <Collapsible
          title="Outline"
          count={headings.length}
          defaultOpen={headings.length > 0}
        >
          {headings.length === 0 ? (
            <div className="px-4 py-1 text-[10px] text-[var(--color-text-muted)]">
              No headings.
            </div>
          ) : (
            <ul>
              {headings.map((h) => (
                <li
                  key={h.uid}
                  className="truncate px-4 py-0.5 text-xs text-[var(--color-text)]"
                  style={{ paddingLeft: `${h.level * 12 + 16}px` }}
                >
                  {h.text}
                </li>
              ))}
            </ul>
          )}
        </Collapsible>

        <Collapsible title="References code" count={0} defaultOpen={false}>
          <div className="px-4 py-1 text-[10px] text-[var(--color-text-muted)]">
            Coming soon.
          </div>
        </Collapsible>
      </div>

      {linkError && <div role="alert" className="mb-2 min-w-0 break-all text-xs text-red-500">{linkError}</div>}

      {/* Bottom: Body */}
      <div
        className={`mb-4 ${
          detailFocus === "source"
            ? "rounded border border-[var(--color-graph-selection)]/40 bg-[var(--color-graph-selection)]/5 p-2"
            : ""
        }`}
      >
        <h3 className="mb-1 text-xs font-semibold uppercase tracking-wide text-[var(--color-text-muted)]">
          Note Evidence
        </h3>
        <div ref={bodyRef} className="min-w-0 rounded border border-[var(--color-border)] bg-[var(--color-surface-alt)] p-3">
          <MarkdownPreview body={body} headings={headings} onWikilink={handleWikilink} />
        </div>
      </div>

      {/* Backlinks */}
      {backlinks.length > 0 && (
        <div
          className={`mb-4 ${
            detailFocus === "related"
              ? "rounded border border-[var(--color-graph-selection)]/40 bg-[var(--color-graph-selection)]/5 p-2"
              : ""
          }`}
        >
          <h3 className="mb-1 text-xs font-semibold uppercase tracking-wide text-[var(--color-text-muted)]">
            Backlinks ({backlinks.length})
          </h3>
          <ul>
            {backlinks.map((bl) => (
              <li key={bl.source_note_uid + bl.source_section_uid}>
                <button
                  type="button"
                  onClick={() => {
                    exploreNode(bl.source_note_uid, "note");
                  }}
                  className="flex w-full items-center gap-2 rounded px-2 py-1 text-left text-xs text-[var(--color-text)] hover:bg-[var(--color-surface-alt)]"
                >
                  <span className="min-w-0 flex-1 truncate">
                    {bl.source_note_title}
                  </span>
                  <span className="shrink-0 text-[10px] text-[var(--color-text-muted)]">
                    {(bl.confidence * 100).toFixed(0)}%
                  </span>
                </button>
              </li>
            ))}
          </ul>
        </div>
      )}

      {/* Unlinked mentions */}
      {unlinked.length > 0 && (
        <div
          className={`mb-4 ${
            detailFocus === "related"
              ? "rounded border border-[var(--color-graph-selection)]/40 bg-[var(--color-graph-selection)]/5 p-2"
              : ""
          }`}
        >
          <h3 className="mb-1 text-xs font-semibold uppercase tracking-wide text-[var(--color-text-muted)]">
            Unlinked mentions ({unlinked.length})
          </h3>
          <ul>
            {unlinked.map((um) => (
              <li key={um.note_uid}>
                <button
                  type="button"
                  onClick={() => {
                    exploreNode(um.note_uid, "note");
                  }}
                  className="w-full rounded px-2 py-1 text-left text-xs hover:bg-[var(--color-surface-alt)]"
                >
                  <div className="truncate font-medium text-[var(--color-text)]">
                    {um.title}
                  </div>
                  <div className="truncate text-[10px] text-[var(--color-text-muted)]">
                    {um.snippet}
                  </div>
                </button>
              </li>
            ))}
          </ul>
        </div>
      )}
    </div>
  );
}
