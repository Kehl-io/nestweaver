import type { StateCreator } from "zustand";
import type { StoreState } from "./index";

export type ExplorerTab = "files" | "symbols" | "notes";

export interface PanelSlice {
  explorerTab: ExplorerTab;
  leftPanelCollapsed: boolean;
  rightPanelCollapsed: boolean;
  setExplorerTab: (tab: ExplorerTab) => void;
  toggleLeftPanel: () => void;
  toggleRightPanel: () => void;
}

export const createPanelSlice: StateCreator<
  StoreState,
  [["zustand/immer", never]],
  [],
  PanelSlice
> = (set) => ({
  explorerTab: "files",
  leftPanelCollapsed: false,
  rightPanelCollapsed: false,

  setExplorerTab: (tab) =>
    set((s) => {
      s.explorerTab = tab;
    }),

  toggleLeftPanel: () =>
    set((s) => {
      s.leftPanelCollapsed = !s.leftPanelCollapsed;
    }),

  toggleRightPanel: () =>
    set((s) => {
      s.rightPanelCollapsed = !s.rightPanelCollapsed;
    }),
});
