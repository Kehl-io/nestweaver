import { useCallback, useEffect, useRef } from "react";
import type Graph from "graphology";
import { preserveGraphLayout } from "../utils/preserveGraphLayout";
import { useStore } from "../../../stores";
import { api } from "../../../api/client";
import { buildGraphFromRepos } from "../utils/buildGraphFromRepos";

export function useReposMode() {
  const setGraphData = useStore((s) => s.setGraphData);
  const graphEpoch = useStore((s) => s.graphEpoch);
  const workspaceId = useStore((s) => s.activeWorkspaceId);
  const previousLayout = useRef<{ workspaceId: string; graph: Graph } | null>(null);
  const sequence = useRef(0);
  const graphMode = useStore((s) => s.graphMode);
  const setActiveLens = useStore((s) => s.setActiveLens);
  const setSceneMetadata = useStore((s) => s.setSceneMetadata);

  const loadReposData = useCallback(async () => {
    const requestId = ++sequence.current;
    if (graphMode !== "repos") return;
    const existingScene = previousLayout.current?.workspaceId === workspaceId &&
      previousLayout.current.graph === useStore.getState().graphInstance;
    if (!existingScene) useStore.getState().clearGraphData();
    if (!existingScene) setActiveLens({ lens: "overview", label: "Repos", targetUid: null, workspaceId });
    if (!existingScene) setSceneMetadata(null);

    try {
      const [repos, services] = await Promise.all([
        api.repos(workspaceId),
        api.services(),
      ]);
      if (requestId !== sequence.current || useStore.getState().graphEpoch !== graphEpoch || useStore.getState().activeWorkspaceId !== workspaceId || useStore.getState().graphMode !== "repos") return;
      const members = new Set(repos.map((repo) => repo.uid));
      const graph = buildGraphFromRepos(repos, services.filter((service) => members.has(service.repo_uid)));
      preserveGraphLayout(graph, existingScene && previousLayout.current ? previousLayout.current.graph : null);
      previousLayout.current = { workspaceId, graph };
      setGraphData(graph);
    } catch (err) {
      console.error("Failed to load repos:", err);
    }
  }, [graphMode, graphEpoch, workspaceId, setGraphData, setActiveLens, setSceneMetadata]);

  useEffect(() => {
    loadReposData();
    return () => { sequence.current += 1; };
  }, [loadReposData]);
}
