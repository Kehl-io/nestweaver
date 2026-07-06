import { create } from "zustand";
import { devtools, persist } from "zustand/middleware";
import { immer } from "zustand/middleware/immer";
import type { GraphSlice } from "./graphSlice";
import { createGraphSlice } from "./graphSlice";
import type { PanelSlice } from "./panelSlice";
import { createPanelSlice } from "./panelSlice";
import type { SearchSlice } from "./searchSlice";
import { createSearchSlice } from "./searchSlice";
import type { AnalysisSlice } from "./analysisSlice";
import { createAnalysisSlice } from "./analysisSlice";
import type { ContentSlice } from "./contentSlice";
import { createContentSlice } from "./contentSlice";
import type { LlmSlice } from "./llmSlice";
import { createLlmSlice } from "./llmSlice";
import type { GraphDataSlice } from "./graphDataSlice";
import { createGraphDataSlice } from "./graphDataSlice";
import type { ShortcutsSlice } from "./shortcutsSlice";
import { createShortcutsSlice } from "./shortcutsSlice";
import type { NotificationSlice } from "./notificationSlice";
import { createNotificationSlice } from "./notificationSlice";
import type { SceneSlice } from "./sceneSlice";
import { createSceneSlice } from "./sceneSlice";
import type { WorkspaceSlice } from "./workspaceSlice";
import { createWorkspaceSlice } from "./workspaceSlice";

function sanitizeViewMode(value: unknown): "graph" | "list" | "matrix" {
  return value === "list" || value === "matrix" ? value : "graph";
}

function sanitizeRepresentationMode(value: unknown): "graph" | "list" | "table" | "matrix" | "json" {
  return value === "graph" ||
    value === "list" ||
    value === "table" ||
    value === "matrix" ||
    value === "json"
    ? value
    : "graph";
}

export type StoreState = GraphSlice &
  WorkspaceSlice &
  SceneSlice &
  PanelSlice &
  SearchSlice &
  AnalysisSlice &
  ContentSlice &
  LlmSlice &
  GraphDataSlice &
  ShortcutsSlice &
  NotificationSlice;

export const useStore = create<StoreState>()(
  devtools(
    persist(
      immer((...a) => ({
        ...createGraphSlice(...a),
        ...createWorkspaceSlice(...a),
        ...createSceneSlice(...a),
        ...createPanelSlice(...a),
        ...createSearchSlice(...a),
        ...createAnalysisSlice(...a),
        ...createContentSlice(...a),
        ...createLlmSlice(...a),
        ...createGraphDataSlice(...a),
        ...createShortcutsSlice(...a),
        ...createNotificationSlice(...a),
      })),
      {
        name: "nestweaver-ui",
        version: 6,
        migrate: (persistedState: any, version: number) => {
          if (persistedState && typeof persistedState === "object") {
            const state = { ...persistedState };
            if (version < 3) {
              delete state.graphMode;
              delete state.layoutMode;
            }
            if (version < 4) {
              state.reducedEffectsUserSet = typeof state.reducedEffects === "boolean";
            }
            if (version < 5) {
              delete state.scopeRepoUid;
              delete state.scopeVaultUid;
              state.activeWorkspaceId = state.activeWorkspaceId ?? "all";
              state.representationMode = state.representationMode ?? state.viewMode ?? "graph";
            }
            if (version < 6) {
              state.representationMode = sanitizeRepresentationMode(
                state.representationMode ?? state.viewMode,
              );
              state.viewMode = sanitizeViewMode(state.viewMode);
            }
            return state;
          }
          return persistedState;
        },
        partialize: (state: any) => ({
          layoutMode: state.layoutMode,
          nodeTypeFilter: state.nodeTypeFilter,
          edgeTypeFilter: state.edgeTypeFilter,
          forceParams: state.forceParams,
          activeStyleRules: state.activeStyleRules,
          theme: state.theme,
          explorerTab: state.explorerTab,
          communityOverlay: state.communityOverlay,
          tagsVisible: state.tagsVisible,
          minimapVisible: state.minimapVisible,
          reducedEffects: state.reducedEffects,
          reducedEffectsUserSet: state.reducedEffectsUserSet,
          activeWorkspaceId: state.activeWorkspaceId,
          representationMode: state.representationMode,
          viewMode: state.viewMode,
        }),
      },
    ),
  ),
);
