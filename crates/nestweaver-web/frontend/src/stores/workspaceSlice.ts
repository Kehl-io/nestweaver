import type { StateCreator } from "zustand";
import { loadWorkspaces as fetchWorkspaces } from "../api/workspaces";
import type {
  SceneMetadata,
  WorkspaceEntry,
  WorkspaceCatalogResponse,
} from "../api/p1Types";
import type { StoreState } from "./index";

export interface WorkspaceSlice {
  workspaces: WorkspaceEntry[];
  activeWorkspaceId: string;
  workspacesLoading: boolean;
  workspacesError: string | null;
  workspacesMeta: SceneMetadata | null;
  selectedWorkspace: () => WorkspaceEntry | null;
  setWorkspaces: (response: WorkspaceCatalogResponse) => void;
  setActiveWorkspaceId: (id: string) => void;
  setWorkspacesLoading: (loading: boolean) => void;
  setWorkspacesError: (error: string | null) => void;
  loadWorkspaces: () => Promise<void>;
  clearWorkspaceError: () => void;
}

function errorMessage(error: unknown): string {
  if (error instanceof Error) return error.message;
  return String(error);
}

export const createWorkspaceSlice: StateCreator<
  StoreState,
  [["zustand/immer", never]],
  [],
  WorkspaceSlice
> = (set, get) => ({
  workspaces: [],
  activeWorkspaceId: "all",
  workspacesLoading: false,
  workspacesError: null,
  workspacesMeta: null,

  selectedWorkspace: () => {
    const state = get();
    return (
      state.workspaces.find((workspace) => workspace.id === state.activeWorkspaceId) ??
      state.workspaces.find((workspace) => workspace.id === "all") ??
      null
    );
  },

  setWorkspaces: (response) =>
    set((s) => {
      s.workspaces = response.workspaces;
      s.workspacesMeta = response._meta;
      if (
        !response.workspaces.some(
          (workspace) => workspace.id === s.activeWorkspaceId,
        )
      ) {
        s.activeWorkspaceId = "all";
      }
    }),

  setActiveWorkspaceId: (id) =>
    set((s) => {
      s.activeWorkspaceId = id;
    }),

  setWorkspacesLoading: (loading) =>
    set((s) => {
      s.workspacesLoading = loading;
    }),

  setWorkspacesError: (error) =>
    set((s) => {
      s.workspacesError = error;
    }),

  loadWorkspaces: async () => {
    set((s) => {
      s.workspacesLoading = true;
      s.workspacesError = null;
    });
    try {
      const response = await fetchWorkspaces();
      set((s) => {
        s.workspaces = response.workspaces;
        s.workspacesMeta = response._meta;
        s.workspacesLoading = false;
        if (
          !response.workspaces.some(
            (workspace) => workspace.id === s.activeWorkspaceId,
          )
        ) {
          s.activeWorkspaceId = "all";
        }
      });
    } catch (error) {
      set((s) => {
        s.workspacesLoading = false;
        s.workspacesError = errorMessage(error);
      });
    }
  },

  clearWorkspaceError: () =>
    set((s) => {
      s.workspacesError = null;
    }),
});
