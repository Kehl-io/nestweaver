import { useCallback, useEffect, useRef, useState } from "react";
import type Graph from "graphology";
import { useStore } from "../stores";

interface WorkerTickMessage {
  type: "tick";
  positions: Float32Array;
}

interface WorkerEndMessage {
  type: "end";
}

type WorkerOutMessage = WorkerTickMessage | WorkerEndMessage;

export interface ForceLayoutControls {
  start: (graphOverride?: Graph) => void;
  stop: () => void;
  kill: () => void;
  isRunning: boolean;
}

export function useForceLayout(): ForceLayoutControls {
  const forceParams = useStore((s) => s.forceParams);
  const setGraphData = useStore((s) => s.setGraphData);

  const workerRef = useRef<Worker | null>(null);
  const [isRunning, setIsRunning] = useState(false);

  const getOrCreateWorker = useCallback((): Worker => {
    if (!workerRef.current) {
      workerRef.current = new Worker(
        new URL("../workers/forceLayoutWorker.ts", import.meta.url),
        { type: "module" },
      );
    }
    return workerRef.current;
  }, []);

  const stop = useCallback(() => {
    if (workerRef.current) {
      workerRef.current.postMessage({ type: "stop" });
    }
    setIsRunning(false);
  }, []);

  const kill = useCallback(() => {
    if (workerRef.current) {
      workerRef.current.terminate();
      workerRef.current = null;
    }
    setIsRunning(false);
  }, []);

  const start = useCallback((graphOverride?: Graph) => {
    const graphInstance = graphOverride ?? useStore.getState().graphInstance;
    if (!graphInstance || graphInstance.order === 0) return;

    const graph = graphInstance;

    const nodes: Array<{ id: string; x: number; y: number }> = [];
    graph.forEachNode((uid, attrs) => {
      nodes.push({
        id: uid,
        x: typeof attrs.x === "number" ? attrs.x : (Math.random() - 0.5) * 100,
        y: typeof attrs.y === "number" ? attrs.y : (Math.random() - 0.5) * 100,
      });
    });

    const links: Array<{ source: string; target: string }> = [];
    graph.forEachEdge((_edge, _attrs, sourceUid, targetUid) => {
      links.push({ source: sourceUid, target: targetUid });
    });

    const nodeIds = nodes.map((n) => n.id);

    const worker = getOrCreateWorker();

    worker.onmessage = (event: MessageEvent<WorkerOutMessage>) => {
      const msg = event.data;

      if (msg.type === "tick") {
        const positions = msg.positions;
        for (let i = 0; i < nodeIds.length; i++) {
          const uid = nodeIds[i];
          if (graph.hasNode(uid)) {
            graph.setNodeAttribute(uid, "x", positions[i * 2 + 0]);
            graph.setNodeAttribute(uid, "y", positions[i * 2 + 1]);
          }
        }
        setGraphData(graph);
        return;
      }

      if (msg.type === "end") {
        setIsRunning(false);
        // Re-frame the settled layout — settling expands scenes past the
        // initial fit, and topology-keyed fitting won't re-run on its own
        useStore.getState().requestCameraFit();
      }
    };

    worker.onerror = (err) => {
      console.error("[useForceLayout] worker error:", err);
      setIsRunning(false);
    };

    worker.postMessage({
      type: "init",
      nodes,
      links,
      options: {
        repulsion: forceParams.repulsion,
        gravity: forceParams.gravity,
        settling: forceParams.settling,
      },
    });

    setIsRunning(true);
  }, [forceParams, getOrCreateWorker, setGraphData]);

  useEffect(() => {
    return () => {
      if (workerRef.current) {
        workerRef.current.terminate();
        workerRef.current = null;
      }
    };
  }, []);

  return { start, stop, kill, isRunning };
}
