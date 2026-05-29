import { useCallback } from "react";
import { UMAP } from "umap-js";
import { useStore } from "../../../stores";

export function useSemanticLayout() {
  const graphInstance = useStore((s) => s.graphInstance);
  const setGraphData = useStore((s) => s.setGraphData);

  const applySemanticLayout = useCallback(() => {
    if (!graphInstance) return;

    const graph = graphInstance;
    const nodes: string[] = [];
    const embeddings: number[][] = [];

    graph.forEachNode((node: string, attrs: Record<string, unknown>) => {
      if (
        attrs.embedding &&
        Array.isArray(attrs.embedding) &&
        attrs.embedding.length > 0
      ) {
        nodes.push(node);
        embeddings.push(attrs.embedding as number[]);
      }
    });

    if (embeddings.length < 5) {
      console.warn(
        "Not enough embeddings for UMAP (need >= 5, got",
        embeddings.length,
        ")",
      );
      return;
    }

    const umap = new UMAP({
      nNeighbors: Math.min(15, embeddings.length - 1),
      minDist: 0.1,
      nComponents: 2,
    });

    const positions = umap.fit(embeddings);

    let minX = Infinity,
      maxX = -Infinity,
      minY = Infinity,
      maxY = -Infinity;
    for (const [x, y] of positions) {
      if (x < minX) minX = x;
      if (x > maxX) maxX = x;
      if (y < minY) minY = y;
      if (y > maxY) maxY = y;
    }
    const rangeX = maxX - minX || 1;
    const rangeY = maxY - minY || 1;
    const scale = 500;

    for (let i = 0; i < nodes.length; i++) {
      const [x, y] = positions[i];
      graph.setNodeAttribute(
        nodes[i],
        "x",
        ((x - minX) / rangeX - 0.5) * scale,
      );
      graph.setNodeAttribute(
        nodes[i],
        "y",
        ((y - minY) / rangeY - 0.5) * scale,
      );
    }

    // Signal that positions have changed so the bridge rebuilds buffers
    setGraphData(graph);
  }, [graphInstance, setGraphData]);

  return { applySemanticLayout };
}
