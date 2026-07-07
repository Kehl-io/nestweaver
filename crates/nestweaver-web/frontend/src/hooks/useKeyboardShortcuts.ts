import { useEffect } from "react";
import { useHotkeys } from "react-hotkeys-hook";
import { useStore } from "../stores";
import type { GraphMode } from "../api/types";
import { useNavigationHistory } from "./useNavigationHistory";

const MODES: GraphMode[] = ["overview", "context", "impact", "repos", "features", "local"];

function isEditableTarget(target: EventTarget | null) {
  if (!(target instanceof HTMLElement)) return false;
  return target.isContentEditable || ["INPUT", "SELECT", "TEXTAREA"].includes(target.tagName);
}

export function useKeyboardShortcuts() {
  const modalOpen = useStore((s) => s.llmBarOpen || s.shortcutsOpen);
  const activeView = useStore((s) => s.activeView);
  const graphViewActive = activeView === "graph";
  const setMode = useStore((s) => s.setGraphMode);
  const toggleLeft = useStore((s) => s.toggleLeftPanel);
  const toggleRight = useStore((s) => s.toggleRightPanel);
  const toggleCommunity = useStore((s) => s.toggleCommunityOverlay);
  const toggleMinimap = useStore((s) => s.toggleMinimap);
  const toggleTags = useStore((s) => s.toggleTags);
  const selectNode = useStore((s) => s.selectNode);
  const toggleViewMode = useStore((s) => s.toggleViewMode);
  const seedReducedEffectsFromSystem = useStore((s) => s.seedReducedEffectsFromSystem);
  const { undo, redo } = useNavigationHistory();
  // Graph-scene hotkeys must not fire under Presentation/Canvas views
  const globalHotkeyOptions = { enabled: !modalOpen && graphViewActive };

  useHotkeys("1", () => setMode(MODES[0]), globalHotkeyOptions);
  useHotkeys("2", () => setMode(MODES[1]), globalHotkeyOptions);
  useHotkeys("3", () => setMode(MODES[2]), globalHotkeyOptions);
  useHotkeys("4", () => setMode(MODES[3]), globalHotkeyOptions);
  useHotkeys("5", () => setMode(MODES[4]), globalHotkeyOptions);
  useHotkeys("6", () => setMode(MODES[5]), globalHotkeyOptions);

  useEffect(() => {
    const motionQuery = window.matchMedia("(prefers-reduced-motion: reduce)");
    const seedReducedEffects = () => seedReducedEffectsFromSystem(motionQuery.matches);

    seedReducedEffects();
    motionQuery.addEventListener("change", seedReducedEffects);
    return () => motionQuery.removeEventListener("change", seedReducedEffects);
  }, [seedReducedEffectsFromSystem]);

  useHotkeys("[", () => toggleLeft(), globalHotkeyOptions);
  useHotkeys("]", () => toggleRight(), globalHotkeyOptions);

  useHotkeys("c", () => toggleCommunity(), globalHotkeyOptions);
  useHotkeys("m", () => toggleMinimap(), globalHotkeyOptions);
  useHotkeys("t", () => toggleTags(), globalHotkeyOptions);

  useHotkeys("escape", () => selectNode(null), globalHotkeyOptions);

  // mod+z — undo navigation. Not enabled on form tags: inside inputs the
  // browser's native text undo must win over scene-history undo.
  useHotkeys(
    "mod+z",
    (e) => {
      e.preventDefault();
      undo();
    },
    { enabled: !modalOpen && graphViewActive },
  );

  // mod+shift+z — redo navigation
  useHotkeys(
    "mod+shift+z",
    (e) => {
      e.preventDefault();
      redo();
    },
    { enabled: !modalOpen && graphViewActive },
  );

  // i — impact analysis for selected node
  useHotkeys("i", () => {
    const id = useStore.getState().selectedNodeId;
    if (id) {
      useStore.getState().selectNode(id, null);
      useStore.getState().setGraphMode("impact");
    }
  }, globalHotkeyOptions);

  // p — find path from selected node
  useHotkeys("p", () => {
    const id = useStore.getState().selectedNodeId;
    if (id) useStore.getState().startPathfinding(id);
  }, globalHotkeyOptions);

  // mod+k — open LLM query bar
  useHotkeys(
    "mod+k",
    (e) => {
      e.preventDefault();
      useStore.getState().openLlmBar();
    },
    { enableOnFormTags: ["INPUT"], enabled: !modalOpen },
  );

  useEffect(() => {
    const handleQuestionMark = (event: KeyboardEvent) => {
      if (event.key !== "?" || isEditableTarget(event.target)) return;

      const state = useStore.getState();
      if (state.llmBarOpen || state.shortcutsOpen) return;

      event.preventDefault();
      state.openShortcuts();
    };

    window.addEventListener("keydown", handleQuestionMark);
    return () => window.removeEventListener("keydown", handleQuestionMark);
  }, []);

  // mod+l — toggle between graph and list view
  useHotkeys(
    "mod+l",
    (e) => {
      e.preventDefault();
      toggleViewMode();
    },
    { enableOnFormTags: ["INPUT"], enabled: !modalOpen && graphViewActive },
  );

  // e — export (no-op; export menu is UI-driven via toolbar button)
  // f — fit to viewport (implement via store action that GraphPanel reads)
  // r — reset layout (implement via store action that GraphPanel reads)
}
