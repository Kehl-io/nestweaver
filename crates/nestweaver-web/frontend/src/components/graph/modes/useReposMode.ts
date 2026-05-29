import { useCallback, useEffect } from "react";
import { useStore } from "../../../stores";
import { api } from "../../../api/client";
import { buildGraphFromRepos } from "../utils/buildGraphFromRepos";

export function useReposMode() {
  const setGraphData = useStore((s) => s.setGraphData);
  const graphMode = useStore((s) => s.graphMode);

  const loadReposData = useCallback(async () => {
    if (graphMode !== "repos") return;

    try {
      const [repos, services] = await Promise.all([
        api.repos(),
        api.services(),
      ]);
      const graph = buildGraphFromRepos(repos, services);
      setGraphData(graph);
    } catch (err) {
      console.error("Failed to load repos:", err);
    }
  }, [graphMode, setGraphData]);

  useEffect(() => {
    loadReposData();
  }, [loadReposData]);
}
