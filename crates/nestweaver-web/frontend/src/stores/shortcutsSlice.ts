import type { StateCreator } from "zustand";
import type { StoreState } from "./index";

export interface ShortcutsSlice {
  shortcutsOpen: boolean;
  openShortcuts: () => void;
  closeShortcuts: () => void;
  toggleShortcuts: () => void;
}

export const createShortcutsSlice: StateCreator<
  StoreState,
  [["zustand/immer", never]],
  [],
  ShortcutsSlice
> = (set) => ({
  shortcutsOpen: false,
  openShortcuts: () =>
    set((s) => {
      s.shortcutsOpen = true;
    }),
  closeShortcuts: () =>
    set((s) => {
      s.shortcutsOpen = false;
    }),
  toggleShortcuts: () =>
    set((s) => {
      s.shortcutsOpen = !s.shortcutsOpen;
    }),
});
