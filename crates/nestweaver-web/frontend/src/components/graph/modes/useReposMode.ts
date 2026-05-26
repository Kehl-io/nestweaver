import { useCallback, useEffect } from "react";
import { useLoadGraph } from "@react-sigma/core";
import { useStore } from "../../../stores";
import { api } from "../../../api/client";
import { buildGraphFromRepos } from "../utils/buildGraphFromRepos";

export function useReposMode() {
  const loadGraph = useLoadGraph();
  const graphMode = useStore((s) => s.graphMode);

  const loadReposData = useCallback(async () => {
    if (graphMode !== "repos") return;

    try {
      const [repos, services] = await Promise.all([
        api.repos(),
        api.services(),
      ]);
      const graph = buildGraphFromRepos(repos, services);
      loadGraph(graph);
    } catch (err) {
      console.error("Failed to load repos:", err);
    }
  }, [graphMode, loadGraph]);

  useEffect(() => {
    loadReposData();
  }, [loadReposData]);
}
