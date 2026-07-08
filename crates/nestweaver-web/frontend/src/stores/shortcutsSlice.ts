import type { StateCreator } from "zustand";
import type { StoreState } from "./index";

let shortcutsFocusReturnTarget: HTMLElement | null = null;

function getFocusReturnTarget(focusReturnTarget?: HTMLElement | null) {
  if (focusReturnTarget !== undefined) return focusReturnTarget;
  if (typeof document === "undefined") return null;

  const activeElement = document.activeElement;
  if (
    activeElement instanceof HTMLElement &&
    activeElement !== document.body &&
    activeElement !== document.documentElement
  ) {
    return activeElement;
  }

  return null;
}

export interface ShortcutsSlice {
  shortcutsOpen: boolean;
  openShortcuts: (focusReturnTarget?: HTMLElement | null) => void;
  closeShortcuts: () => void;
  getShortcutsFocusReturnTarget: () => HTMLElement | null;
  clearShortcutsFocusReturnTarget: () => void;
  toggleShortcuts: () => void;
}

export const createShortcutsSlice: StateCreator<
  StoreState,
  [["zustand/immer", never]],
  [],
  ShortcutsSlice
> = (set) => ({
  shortcutsOpen: false,
  openShortcuts: (focusReturnTarget) => {
    shortcutsFocusReturnTarget = getFocusReturnTarget(focusReturnTarget);
    set((s) => {
      s.shortcutsOpen = true;
    });
  },
  closeShortcuts: () =>
    set((s) => {
      s.shortcutsOpen = false;
    }),
  getShortcutsFocusReturnTarget: () => shortcutsFocusReturnTarget,
  clearShortcutsFocusReturnTarget: () => {
    shortcutsFocusReturnTarget = null;
  },
  toggleShortcuts: () => {
    set((s) => {
      const nextOpen = !s.shortcutsOpen;
      if (nextOpen) {
        shortcutsFocusReturnTarget = getFocusReturnTarget();
      }
      s.shortcutsOpen = nextOpen;
    });
  },
});
