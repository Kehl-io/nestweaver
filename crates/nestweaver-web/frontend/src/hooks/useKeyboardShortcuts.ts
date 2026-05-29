import { useHotkeys } from "react-hotkeys-hook";
import { useStore } from "../stores";
import type { GraphMode } from "../api/types";
import { useNavigationHistory } from "./useNavigationHistory";

const MODES: GraphMode[] = ["context", "impact", "repos", "features", "inspector"];

export function useKeyboardShortcuts() {
  const setMode = useStore((s) => s.setGraphMode);
  const toggleLeft = useStore((s) => s.toggleLeftPanel);
  const toggleRight = useStore((s) => s.toggleRightPanel);
  const toggleCommunity = useStore((s) => s.toggleCommunityOverlay);
  const toggleMinimap = useStore((s) => s.toggleMinimap);
  const toggleTags = useStore((s) => s.toggleTags);
  const selectNode = useStore((s) => s.selectNode);
  const toggleViewMode = useStore((s) => s.toggleViewMode);
  const { undo, redo } = useNavigationHistory();

  useHotkeys("1", () => setMode(MODES[0]));
  useHotkeys("2", () => setMode(MODES[1]));
  useHotkeys("3", () => setMode(MODES[2]));
  useHotkeys("4", () => setMode(MODES[3]));
  useHotkeys("5", () => setMode(MODES[4]));

  useHotkeys("[", () => toggleLeft());
  useHotkeys("]", () => toggleRight());

  useHotkeys("c", () => toggleCommunity());
  useHotkeys("m", () => toggleMinimap());
  useHotkeys("t", () => toggleTags());

  useHotkeys("escape", () => selectNode(null));

  // mod+z — undo navigation
  useHotkeys(
    "mod+z",
    (e) => {
      e.preventDefault();
      undo();
    },
    { enableOnFormTags: ["INPUT"] },
  );

  // mod+shift+z — redo navigation
  useHotkeys(
    "mod+shift+z",
    (e) => {
      e.preventDefault();
      redo();
    },
    { enableOnFormTags: ["INPUT"] },
  );

  // i — impact analysis for selected node
  useHotkeys("i", () => {
    const id = useStore.getState().selectedNodeId;
    if (id) {
      useStore.getState().selectNode(id, null);
      useStore.getState().setGraphMode("impact");
    }
  });

  // p — find path from selected node
  useHotkeys("p", () => {
    const id = useStore.getState().selectedNodeId;
    if (id) useStore.getState().startPathfinding(id);
  });

  // mod+k — open LLM query bar
  useHotkeys(
    "mod+k",
    (e) => {
      e.preventDefault();
      useStore.getState().openLlmBar();
    },
    { enableOnFormTags: ["INPUT"] },
  );

  // mod+l — toggle between graph and list view
  useHotkeys(
    "mod+l",
    (e) => {
      e.preventDefault();
      toggleViewMode();
    },
    { enableOnFormTags: ["INPUT"] },
  );

  // e — export (no-op; export menu is UI-driven via toolbar button)
  // f — fit to viewport (implement via store action that GraphPanel reads)
  // r — reset layout (implement via store action that GraphPanel reads)
}
