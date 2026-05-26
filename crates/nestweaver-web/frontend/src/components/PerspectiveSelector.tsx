import { useEffect, useRef, useState } from "react";
import { api } from "../api/client";
import type { Perspective } from "../api/types";
import { useStore } from "../stores";

export function PerspectiveSelector() {
  const [open, setOpen] = useState(false);
  const [saving, setSaving] = useState(false);
  const [newName, setNewName] = useState("");
  const containerRef = useRef<HTMLDivElement>(null);

  const perspectives = useStore((s) => s.perspectives);
  const activePerspectiveId = useStore((s) => s.activePerspectiveId);
  const setPerspectives = useStore((s) => s.setPerspectives);
  const setActivePerspectiveId = useStore((s) => s.setActivePerspectiveId);

  const graphMode = useStore((s) => s.graphMode);
  const seeds = useStore((s) => s.seeds);
  const scopeFilter = useStore((s) => s.scopeFilter);
  const communityOverlay = useStore((s) => s.communityOverlay);
  const tagsVisible = useStore((s) => s.tagsVisible);
  const minimapVisible = useStore((s) => s.minimapVisible);

  const setGraphMode = useStore((s) => s.setGraphMode);
  const setSeeds = useStore((s) => s.setSeeds);
  const setScopeFilter = useStore((s) => s.setScopeFilter);
  const toggleCommunityOverlay = useStore((s) => s.toggleCommunityOverlay);
  const toggleTags = useStore((s) => s.toggleTags);
  const toggleMinimap = useStore((s) => s.toggleMinimap);

  useEffect(() => {
    api.perspectives().then(setPerspectives).catch(() => {});
  }, [setPerspectives]);

  useEffect(() => {
    function handleClickOutside(e: MouseEvent) {
      if (
        containerRef.current &&
        !containerRef.current.contains(e.target as Node)
      ) {
        setOpen(false);
        setSaving(false);
        setNewName("");
      }
    }
    document.addEventListener("mousedown", handleClickOutside);
    return () => document.removeEventListener("mousedown", handleClickOutside);
  }, []);

  const activePerspective = perspectives.find(
    (p) => p.id === activePerspectiveId,
  );

  function restorePerspective(p: Perspective) {
    const cfg = p.config;

    if (cfg.mode != null) {
      setGraphMode(cfg.mode as typeof graphMode);
    }
    if (Array.isArray(cfg.seeds)) {
      setSeeds(cfg.seeds as string[]);
    }
    if (cfg.scope != null) {
      setScopeFilter(cfg.scope as typeof scopeFilter);
    }
    if (typeof cfg.communityOverlay === "boolean" && cfg.communityOverlay !== communityOverlay) {
      toggleCommunityOverlay();
    }
    if (typeof cfg.tagsVisible === "boolean" && cfg.tagsVisible !== tagsVisible) {
      toggleTags();
    }
    if (typeof cfg.minimapVisible === "boolean" && cfg.minimapVisible !== minimapVisible) {
      toggleMinimap();
    }

    setActivePerspectiveId(p.id);
    setOpen(false);
  }

  async function deletePerspective(id: string) {
    try {
      await fetch(`/api/v1/perspectives/${encodeURIComponent(id)}`, {
        method: "DELETE",
      });
      setPerspectives(perspectives.filter((p) => p.id !== id));
      if (activePerspectiveId === id) {
        setActivePerspectiveId(null);
      }
    } catch {
      // ignore
    }
  }

  async function saveCurrentView() {
    const name = newName.trim();
    if (!name) return;

    try {
      const p = await api.createPerspective(name, {
        mode: graphMode,
        seeds,
        scope: scopeFilter,
        communityOverlay,
        tagsVisible,
        minimapVisible,
      });
      setPerspectives([...perspectives, p]);
      setActivePerspectiveId(p.id);
      setNewName("");
      setSaving(false);
      setOpen(false);
    } catch {
      // ignore
    }
  }

  return (
    <div ref={containerRef} className="relative">
      <button
        type="button"
        onClick={() => setOpen(!open)}
        className="rounded border border-[var(--color-border)] bg-[var(--color-surface-alt)] px-2 py-1.5 text-xs outline-none hover:bg-[var(--color-surface)]"
      >
        {activePerspective ? activePerspective.name : "Perspectives"}
      </button>

      {open && (
        <div className="absolute right-0 top-full z-50 mt-1 w-56 rounded-md border border-[var(--color-border)] bg-[var(--color-surface)] shadow-lg">
          {perspectives.length === 0 && (
            <div className="px-3 py-2 text-sm text-[var(--color-text-muted)]">
              No saved perspectives
            </div>
          )}

          {perspectives.map((p) => (
            <div
              key={p.id}
              className="group flex items-center hover:bg-[var(--color-surface-alt)]"
            >
              <button
                type="button"
                onClick={() => restorePerspective(p)}
                className="flex-1 px-3 py-1.5 text-left text-sm"
              >
                {p.name}
              </button>
              <button
                type="button"
                onClick={() => deletePerspective(p.id)}
                className="mr-2 hidden px-1 text-xs text-[var(--color-text-muted)] hover:text-red-400 group-hover:inline-block"
                title="Delete perspective"
              >
                X
              </button>
            </div>
          ))}

          <div className="border-t border-[var(--color-border)]">
            {saving ? (
              <div className="flex items-center gap-1 px-3 py-1.5">
                <input
                  type="text"
                  value={newName}
                  onChange={(e) => setNewName(e.target.value)}
                  onKeyDown={(e) => {
                    if (e.key === "Enter") saveCurrentView();
                    if (e.key === "Escape") {
                      setSaving(false);
                      setNewName("");
                    }
                  }}
                  placeholder="Name..."
                  autoFocus
                  className="min-w-0 flex-1 rounded border border-[var(--color-border)] bg-[var(--color-surface-alt)] px-2 py-1 text-xs outline-none focus:border-blue-500"
                />
                <button
                  type="button"
                  onClick={saveCurrentView}
                  className="rounded bg-blue-600 px-2 py-1 text-xs text-white hover:bg-blue-500"
                >
                  Save
                </button>
              </div>
            ) : (
              <button
                type="button"
                onClick={() => setSaving(true)}
                className="w-full px-3 py-1.5 text-left text-sm text-blue-400 hover:bg-[var(--color-surface-alt)]"
              >
                Save current view
              </button>
            )}
          </div>
        </div>
      )}
    </div>
  );
}
