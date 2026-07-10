import { useEffect, useMemo, useState } from "react";
import { api } from "../../api/client";
import type { Repo, SymbolCandidate } from "../../api/types";
import { useStore } from "../../stores";
import { Collapsible } from "../shared/Collapsible";

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

function FileTreeNode({
  node,
  depth,
  onSelect,
}: {
  node: TreeNode;
  depth: number;
  onSelect: (path: string) => void;
}) {
  const [open, setOpen] = useState(depth < 2);

  const sortedChildren = useMemo(() => {
    const arr = Array.from(node.children.values());
    arr.sort((a, b) => {
      if (a.isFile !== b.isFile) return a.isFile ? 1 : -1;
      return a.name.localeCompare(b.name);
    });
    return arr;
  }, [node.children]);

  if (node.isFile) {
    return (
      <button
        type="button"
        onClick={() => onSelect(node.fullPath)}
        className="flex w-full items-center gap-1 py-0.5 text-left text-xs text-[var(--color-text)] hover:bg-[var(--color-surface-alt)]"
        style={{ paddingLeft: `${depth * 12 + 8}px` }}
      >
        <span className="text-[var(--color-text-muted)]">&#9702;</span>
        <span className="truncate">{node.name}</span>
      </button>
    );
  }

  return (
    <div>
      <button
        type="button"
        onClick={() => setOpen((v) => !v)}
        className="flex w-full items-center gap-1 py-0.5 text-left text-xs font-medium text-[var(--color-text)] hover:bg-[var(--color-surface-alt)]"
        style={{ paddingLeft: `${depth * 12 + 8}px` }}
      >
        <span className="w-3 text-center text-[var(--color-text-muted)]">
          {open ? "▾" : "▸"}
        </span>
        <span className="truncate">{node.name}</span>
      </button>
      {open &&
        sortedChildren.map((child) => (
          <FileTreeNode
            key={child.fullPath}
            node={child}
            depth={depth + 1}
            onSelect={onSelect}
          />
        ))}
    </div>
  );
}

export function FilesTab() {
  const selectNode = useStore((s) => s.selectNode);

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
    <div className="flex h-full flex-col overflow-y-auto">
      {repoTrees.map(({ repo, repoName, tree }) => (
        <Collapsible
          key={repo.uid}
          title={repoName}
          count={tree.children.size}
          defaultOpen
        >
          <div className="pb-1">
            {repo.staleness_commits_behind > 0 && (
              <div className="mx-2 mb-1 rounded bg-yellow-500/10 px-2 py-0.5 text-[10px] text-yellow-600">
                {repo.staleness_commits_behind} commit
                {repo.staleness_commits_behind !== 1 ? "s" : ""} behind
              </div>
            )}
            {Array.from(tree.children.values())
              .sort((a, b) => {
                if (a.isFile !== b.isFile) return a.isFile ? 1 : -1;
                return a.name.localeCompare(b.name);
              })
              .map((child) => (
                <FileTreeNode
                  key={child.fullPath}
                  node={child}
                  depth={1}
                  onSelect={handleSelect}
                />
              ))}
          </div>
        </Collapsible>
      ))}
    </div>
  );
}
