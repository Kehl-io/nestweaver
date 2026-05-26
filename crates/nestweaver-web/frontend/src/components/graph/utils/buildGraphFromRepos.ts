import Graph from "graphology";
import type { Repo, Service } from "../../../api/types";
import { nodeSize } from "./graphColors";

export function buildGraphFromRepos(
  repos: Repo[],
  services: Service[],
): Graph {
  const graph = new Graph({ type: "directed", multi: true });

  for (let i = 0; i < repos.length; i++) {
    const repo = repos[i];
    const name = repo.url.split("/").pop() || repo.uid;
    const angle = (i / Math.max(repos.length, 1)) * Math.PI * 2;

    graph.addNode(repo.uid, {
      label: name,
      x: Math.cos(angle) * 200,
      y: Math.sin(angle) * 200,
      size: 30,
      color: "#6B7280",
      kind: "Repo",
      forceLabel: true,
    });
  }

  for (const svc of services) {
    if (graph.hasNode(svc.uid)) continue;

    const parentX = graph.hasNode(svc.repo_uid)
      ? (graph.getNodeAttribute(svc.repo_uid, "x") as number)
      : 0;
    const parentY = graph.hasNode(svc.repo_uid)
      ? (graph.getNodeAttribute(svc.repo_uid, "y") as number)
      : 0;

    graph.addNode(svc.uid, {
      label: svc.name,
      x: parentX + (Math.random() - 0.5) * 80,
      y: parentY + (Math.random() - 0.5) * 80,
      size: 15,
      color: "#3B82F6",
      kind: "Service",
      repoUid: svc.repo_uid,
    });

    if (graph.hasNode(svc.repo_uid)) {
      graph.addEdge(svc.repo_uid, svc.uid, {
        type: "arrow",
        size: 1,
        color: "#D1D5DB",
        label: "contains",
      });
    }
  }

  // Second pass: update node sizes based on actual degree
  graph.forEachNode((nodeId) => {
    graph.setNodeAttribute(nodeId, "size", nodeSize(graph.degree(nodeId), 0.001));
  });

  return graph;
}
