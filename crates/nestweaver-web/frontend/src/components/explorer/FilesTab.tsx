import { useEffect, useMemo, useRef, useState } from "react";
import { api } from "../../api/client";
import type { Repo, SymbolCandidate } from "../../api/types";
import { useStore } from "../../stores";
import { GLOBAL_SINGLE_KEYS } from "../../hooks/useKeyboardShortcuts";

interface TreeNode {
  name: string;
  fullPath: string;
  children: Map<string, TreeNode>;
  isFile: boolean;
}

function buildTree(paths: string[]): TreeNode {
  const root: TreeNode = {
    name: "",
    fullPath: "",
    children: new Map(),
    isFile: false,
  };
  for (const p of paths) {
    const parts = p.split("/").filter(Boolean);
    let cur = root;
    for (let i = 0; i < parts.length; i++) {
      const part = parts[i];
      if (!cur.children.has(part)) {
        cur.children.set(part, {
          name: part,
          fullPath: parts.slice(0, i + 1).join("/"),
          children: new Map(),
          isFile: i === parts.length - 1,
        });
      }
      cur = cur.children.get(part)!;
    }
  }
  return root;
}

function sortedChildren(node: TreeNode): TreeNode[] {
  return Array.from(node.children.values()).sort((a, b) => {
    if (a.isFile !== b.isFile) return a.isFile ? 1 : -1;
    return a.name.localeCompare(b.name);
  });
}

/** One visible row of the flattened tree (WAI-ARIA tree, flat markup). */
interface TreeRow {
  id: string;
  label: string;
  level: number;
  parentId: string | null;
  setSize: number;
  posInSet: number;
  expandable: boolean;
  expanded: boolean;
  filePath: string | null;
  badge?: string;
}

function initialExpanded(
  repoTrees: { repo: Repo; tree: TreeNode }[],
): Set<string> {
  // Same default as before: repos and their top-level folders open.
  const open = new Set<string>();
  for (const { repo, tree } of repoTrees) {
    open.add(`repo:${repo.uid}`);
    for (const child of tree.children.values()) {
      if (!child.isFile) open.add(`dir:${repo.uid}:${child.fullPath}`);
    }
  }
  return open;
}

function visibleRows(
  repoTrees: { repo: Repo; repoName: string; tree: TreeNode }[],
  expanded: Set<string>,
): TreeRow[] {
  const rows: TreeRow[] = [];
  const walk = (
    repoUid: string,
    nodes: TreeNode[],
    level: number,
    parentId: string,
  ) => {
    nodes.forEach((node, index) => {
      const id = node.isFile
        ? `file:${repoUid}:${node.fullPath}`
        : `dir:${repoUid}:${node.fullPath}`;
      const open = !node.isFile && expanded.has(id);
      rows.push({
        id,
        label: node.name,
        level,
        parentId,
        setSize: nodes.length,
        posInSet: index + 1,
        expandable: !node.isFile,
        expanded: open,
        filePath: node.isFile ? node.fullPath : null,
      });
      if (open) walk(repoUid, sortedChildren(node), level + 1, id);
    });
  };
  repoTrees.forEach(({ repo, repoName, tree }, index) => {
    const id = `repo:${repo.uid}`;
    const open = expanded.has(id);
    const behind = repo.staleness_commits_behind;
    rows.push({
      id,
      label: repoName,
      level: 1,
      parentId: null,
      setSize: repoTrees.length,
      posInSet: index + 1,
      expandable: true,
      expanded: open,
      filePath: null,
      badge: behind > 0 ? `${behind} commit${behind !== 1 ? "s" : ""} behind` : undefined,
    });
    if (open) walk(repo.uid, sortedChildren(tree), 2, id);
  });
  return rows;
}

export function FilesTab() {
  const selectNode = useStore((s) => s.selectNode);
  const selectedNodeId = useStore((s) => s.selectedNodeId);
  const selectedKind = useStore((s) => s.selectedNodeKind);

  const [repos, setRepos] = useState<Repo[]>([]);
  const [symbols, setSymbols] = useState<SymbolCandidate[]>([]);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    setLoading(true);
    setError(null);
    Promise.all([api.repos(), api.symbolsTop(500)])
      .then(([r, s]) => {
        setRepos(r);
        setSymbols(s);
      })
      .catch((e) => setError(e.message ?? "Failed to load files"))
      .finally(() => setLoading(false));
  }, []);

  const repoTrees = useMemo(() => {
    const byRepo = new Map<string, Set<string>>();
    for (const sym of symbols) {
      // Bucket by the symbol's repo_uid (colon-delimited, matches Repo.uid).
      // Previously this split the uid on "::" — which symbol uids never
      // contain — so every file landed in a bogus bucket and every repo's
      // tree rendered empty (0 files).
      const repoId = sym.repo_uid || "__default__";
      if (!byRepo.has(repoId)) byRepo.set(repoId, new Set());
      byRepo.get(repoId)!.add(sym.file_path);
    }

    return repos.map((repo) => {
      const paths = byRepo.get(repo.uid);
      const tree = paths ? buildTree(Array.from(paths)) : buildTree([]);
      const repoName = repo.url.split("/").pop() ?? repo.url;
      return { repo, repoName, tree };
    });
  }, [repos, symbols]);

  const handleSelect = (path: string) => {
    selectNode(path, "file");
  };

  if (loading) {
    return (
      <div className="flex h-full items-center justify-center text-sm text-[var(--color-text-muted)]">
        Loading files...
      </div>
    );
  }

  if (error) {
    return (
      <div className="flex h-full items-center justify-center p-4 text-sm text-red-500">
        {error}
      </div>
    );
  }

  if (repos.length === 0) {
    return (
      <div className="flex h-full items-center justify-center p-4 text-sm text-[var(--color-text-muted)]">
        No repos indexed.
      </div>
    );
  }

  return (
    <FileTree
      repoTrees={repoTrees}
      selectedPath={selectedKind === "file" ? selectedNodeId : null}
      onSelect={handleSelect}
    />
  );
}

/**
 * nw-565: the Files tree is one Tab stop (roving tabindex) navigated with
 * the WAI-ARIA tree keys: Up/Down move, Right opens or enters, Left closes
 * or climbs, Home/End jump, Enter/Space opens a file or toggles a folder,
 * and a printable key jumps to the next item starting with it.
 */
function FileTree({
  repoTrees,
  selectedPath,
  onSelect,
}: {
  repoTrees: { repo: Repo; repoName: string; tree: TreeNode }[];
  selectedPath: string | null;
  onSelect: (path: string) => void;
}) {
  const [expanded, setExpanded] = useState(() => initialExpanded(repoTrees));
  const [activeId, setActiveId] = useState<string | null>(null);
  const itemRefs = useRef(new Map<string, HTMLDivElement>());
  const rows = useMemo(() => visibleRows(repoTrees, expanded), [repoTrees, expanded]);
  const current = rows.find((row) => row.id === activeId) ?? rows[0] ?? null;

  const moveTo = (row: TreeRow | undefined) => {
    if (!row) return;
    setActiveId(row.id);
    itemRefs.current.get(row.id)?.focus();
  };

  const toggle = (row: TreeRow, open: boolean) => {
    setExpanded((prev) => {
      const next = new Set(prev);
      if (open) next.add(row.id);
      else next.delete(row.id);
      return next;
    });
  };

  const activate = (row: TreeRow) => {
    setActiveId(row.id);
    if (row.filePath) onSelect(row.filePath);
    else toggle(row, !row.expanded);
  };

  const onKeyDown = (event: React.KeyboardEvent<HTMLDivElement>, row: TreeRow) => {
    const index = rows.findIndex((candidate) => candidate.id === row.id);
    switch (event.key) {
      case "ArrowDown":
        moveTo(rows[index + 1]);
        break;
      case "ArrowUp":
        moveTo(rows[index - 1]);
        break;
      case "ArrowRight":
        if (row.expandable && !row.expanded) toggle(row, true);
        else if (row.expandable && rows[index + 1]?.parentId === row.id) moveTo(rows[index + 1]);
        break;
      case "ArrowLeft":
        if (row.expandable && row.expanded) toggle(row, false);
        else moveTo(rows.find((candidate) => candidate.id === row.parentId));
        break;
      case "Home":
        moveTo(rows[0]);
        break;
      case "End":
        moveTo(rows[rows.length - 1]);
        break;
      case "Enter":
      case " ":
        activate(row);
        break;
      default:
        // Type-ahead: an unmodified letter or digit that is not an app-wide
        // shortcut. Everything else (/, ?, c, m, t, digits 1-6, modified
        // keys) passes through to the global handlers.
        if (
          /^[a-z0-9]$/i.test(event.key) &&
          !GLOBAL_SINGLE_KEYS.has(event.key.toLowerCase()) &&
          !event.altKey &&
          !event.metaKey &&
          !event.ctrlKey
        ) {
          const key = event.key.toLowerCase();
          const ordered = [...rows.slice(index + 1), ...rows.slice(0, index)];
          moveTo(ordered.find((candidate) => candidate.label.toLowerCase().startsWith(key)));
          break;
        }
        return;
    }
    event.preventDefault();
    event.stopPropagation();
  };

  return (
    <div
      role="tree"
      aria-label="Files"
      className="flex h-full flex-col overflow-y-auto py-1"
    >
      {rows.map((row) => {
        const isRepo = row.level === 1;
        const selected = row.filePath != null && row.filePath === selectedPath;
        return (
          <div
            key={row.id}
            ref={(el) => {
              if (el) itemRefs.current.set(row.id, el);
              else itemRefs.current.delete(row.id);
            }}
            role="treeitem"
            aria-level={row.level}
            aria-setsize={row.setSize}
            aria-posinset={row.posInSet}
            aria-expanded={row.expandable ? row.expanded : undefined}
            aria-selected={row.filePath ? selected : undefined}
            tabIndex={row.id === current?.id ? 0 : -1}
            onClick={() => activate(row)}
            onFocus={() => setActiveId(row.id)}
            onKeyDown={(event) => onKeyDown(event, row)}
            title={row.filePath ?? undefined}
            className={`flex w-full cursor-pointer items-center gap-1 py-0.5 pr-2 text-left text-xs outline-none hover:bg-[var(--color-surface-alt)] focus-visible:ring-2 focus-visible:ring-inset focus-visible:ring-[var(--color-graph-selection)] ${
              isRepo
                ? "font-semibold uppercase tracking-wide text-[var(--color-text-muted)]"
                : row.expandable
                  ? "font-medium text-[var(--color-text)]"
                  : "text-[var(--color-text)]"
            } ${selected ? "bg-[var(--color-surface-alt)] text-[var(--color-graph-selection)]" : ""}`}
            style={{ paddingLeft: `${(row.level - 1) * 12 + 8}px` }}
          >
            <span className="w-3 shrink-0 text-center text-[var(--color-text-muted)]" aria-hidden="true">
              {row.expandable ? (row.expanded ? "▾" : "▸") : "◦"}
            </span>
            <span className="truncate">{row.label}</span>
            {row.badge && (
              <span className="ml-auto shrink-0 rounded bg-yellow-500/10 px-1.5 py-0.5 text-[10px] font-normal normal-case tracking-normal text-yellow-600">
                {row.badge}
              </span>
            )}
          </div>
        );
      })}
    </div>
  );
}
