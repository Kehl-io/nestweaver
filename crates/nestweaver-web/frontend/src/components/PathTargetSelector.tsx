import { useId, useState } from "react";
import { useStore } from "../stores";
import { api } from "../api/client";
import type { SymbolCandidate } from "../api/types";
import { useSymbolQuery } from "../hooks/useSymbolQuery";

// A graph uid carries a type prefix; anything else is a name to resolve.
const UID_PREFIX = /^(sym|note|repo|svc|tag|vlt|hdg|sec|file):/;

function uidTail(uid: string): string {
  const tail = uid.split(":").pop() ?? uid;
  return /^\d+$/.test(tail) ? uid : tail;
}

function useNodeName(uid: string | null): string | null {
  const graph = useStore((s) => s.graphInstance);
  const symbol = useSymbolQuery(uid?.startsWith("sym:") ? uid : null);
  if (!uid) return null;
  if (graph?.hasNode(uid)) {
    const label = graph.getNodeAttribute(uid, "label");
    if (typeof label === "string" && label) return label;
  }
  if (symbol.detail) return symbol.detail.symbol.name;
  return uidTail(uid);
}

type Resolution =
  | { state: "idle" }
  | { state: "resolving" }
  | { state: "choose"; query: string; exact: boolean; candidates: SymbolCandidate[] }
  | { state: "error"; message: string };

export function PathTargetSelector() {
  const [target, setTarget] = useState("");
  const [resolution, setResolution] = useState<Resolution>({ state: "idle" });
  const pathfindingFrom = useStore((s) => s.pathfindingFrom);
  const pathStatus = useStore((s) => s.pathStatus);
  const setPathfindingTarget = useStore((s) => s.setPathfindingTarget);
  const setPathResults = useStore((s) => s.setPathResults);
  const setPathError = useStore((s) => s.setPathError);
  const clearPathfinding = useStore((s) => s.clearPathfinding);
  const fromName = useNodeName(pathfindingFrom);
  const titleId = useId();
  const listId = useId();
  const pending = pathStatus === "pending" || resolution.state === "resolving";

  const findPath = async (targetUid: string) => {
    if (!pathfindingFrom) return;
    const request = setPathfindingTarget(targetUid);
    try {
      const results = await api.paths(pathfindingFrom, targetUid, 5, 10);
      setPathResults(results, request);
    } catch (error) {
      setPathError(
        error instanceof Error && error.message
          ? error.message
          : "Path query failed.",
        request,
      );
    }
  };

  const choose = (uid: string) => {
    setResolution({ state: "idle" });
    void findPath(uid);
  };

  // nw-568: a name is resolved the way search resolves it. One exact match
  // is used directly; several (or only near matches) are offered as a
  // choice; none is reported instead of querying a literal path segment.
  const handleSubmit = async () => {
    const trimmedTarget = target.trim();
    if (!trimmedTarget || !pathfindingFrom || pending) return;
    if (UID_PREFIX.test(trimmedTarget)) {
      await findPath(trimmedTarget);
      return;
    }
    setResolution({ state: "resolving" });
    let hits: SymbolCandidate[];
    try {
      hits = await api.search(trimmedTarget, 20);
    } catch (error) {
      setResolution({
        state: "error",
        message:
          error instanceof Error && error.message ? error.message : "Name lookup failed.",
      });
      return;
    }
    const others = hits.filter((hit) => hit.uid !== pathfindingFrom);
    const caseSensitive = others.filter((hit) => hit.name === trimmedTarget);
    const exact = caseSensitive.length > 0
      ? caseSensitive
      : others.filter((hit) => hit.name.toLowerCase() === trimmedTarget.toLowerCase());
    if (exact.length === 1) {
      choose(exact[0].uid);
    } else if (exact.length > 1) {
      setResolution({ state: "choose", query: trimmedTarget, exact: true, candidates: exact });
    } else if (others.length > 0) {
      setResolution({
        state: "choose",
        query: trimmedTarget,
        exact: false,
        candidates: others.slice(0, 8),
      });
    } else {
      setResolution({ state: "error", message: `No node named "${trimmedTarget}".` });
    }
  };

  return (
    <div
      role="dialog"
      aria-labelledby={titleId}
      className="absolute top-1/2 left-1/2 -translate-x-1/2 -translate-y-1/2 z-50 bg-[var(--color-surface)] border border-[var(--color-border)] rounded-lg shadow-xl p-4 min-w-72 max-w-[min(28rem,calc(100%-2rem))]"
    >
      <h3 id={titleId} className="text-sm font-semibold mb-2">Find path</h3>
      <p className="text-xs text-[var(--color-text-muted)] mb-3">
        From:{" "}
        <span
          className="bg-[var(--color-surface-alt)] px-1 rounded font-medium text-[var(--color-text)]"
          title={pathfindingFrom ?? undefined}
        >
          {fromName}
        </span>
      </p>
      <div className="flex gap-2">
        <input
          type="text"
          value={target}
          onChange={(e) => {
            setTarget(e.target.value);
            if (resolution.state !== "idle") setResolution({ state: "idle" });
          }}
          onKeyDown={(e) => e.key === "Enter" && handleSubmit()}
          placeholder="Target name or UID..."
          aria-label="Path target"
          aria-controls={resolution.state === "choose" ? listId : undefined}
          className="flex-1 h-8 px-2 text-sm border border-[var(--color-border)] rounded bg-[var(--color-surface)] outline-none focus:ring-2 focus:ring-[var(--color-graph-selection)]"
          disabled={pending}
          autoFocus
        />
        <button
          onClick={handleSubmit}
          disabled={pending}
          className="h-8 px-3 text-xs bg-[var(--color-graph-selection)] text-white rounded opacity-90 hover:opacity-100"
        >
          {pending ? "Finding" : "Find"}
        </button>
      </div>
      {resolution.state === "error" && (
        <p role="alert" className="mt-2 text-xs text-amber-300">
          {resolution.message}
        </p>
      )}
      {resolution.state === "choose" && (
        <div className="mt-2">
          <p className="mb-1 text-xs text-[var(--color-text-muted)]">
            {resolution.exact
              ? `${resolution.candidates.length} nodes are named "${resolution.query}". Choose one:`
              : `No node is named "${resolution.query}". Closest matches:`}
          </p>
          <ul
            id={listId}
            role="listbox"
            aria-label="Matching nodes"
            className="max-h-48 overflow-y-auto rounded border border-[var(--color-border)]"
          >
            {resolution.candidates.map((candidate) => (
              <li
                key={candidate.uid}
                role="option"
                aria-selected={false}
                tabIndex={0}
                title={candidate.uid}
                onClick={() => choose(candidate.uid)}
                onKeyDown={(e) => {
                  if (e.key === "Enter" || e.key === " ") {
                    e.preventDefault();
                    choose(candidate.uid);
                  } else if (e.key === "ArrowDown" || e.key === "ArrowUp") {
                    e.preventDefault();
                    const sibling = e.key === "ArrowDown"
                      ? e.currentTarget.nextElementSibling
                      : e.currentTarget.previousElementSibling;
                    (sibling as HTMLElement | null)?.focus();
                  }
                }}
                className="cursor-pointer px-2 py-1.5 text-xs outline-none hover:bg-[var(--color-surface-alt)] focus:bg-[var(--color-surface-alt)] focus-visible:ring-2 focus-visible:ring-inset focus-visible:ring-[var(--color-graph-selection)]"
              >
                <span className="font-medium text-[var(--color-text)]">{candidate.name}</span>{" "}
                <span className="text-[var(--color-text-muted)]">
                  {candidate.kind} · {candidate.file_path}:{candidate.start_line}
                </span>
              </li>
            ))}
          </ul>
        </div>
      )}
      <button
        onClick={clearPathfinding}
        className="mt-2 text-xs text-[var(--color-text-muted)] hover:text-[var(--color-text)]"
      >
        Cancel
      </button>
    </div>
  );
}
