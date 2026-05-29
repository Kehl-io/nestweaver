import {
  forceSimulation,
  forceLink,
  forceManyBody,
  forceCenter,
  type SimulationNodeDatum,
  type SimulationLinkDatum,
  type Simulation,
} from "d3-force-3d";

interface InitOptions {
  repulsion: number;
  gravity: number;
  settling: number;
}

interface NodeDatum extends SimulationNodeDatum {
  id: string;
}

interface LinkDatum extends SimulationLinkDatum<NodeDatum> {
  source: string | NodeDatum;
  target: string | NodeDatum;
}

type InMessage =
  | {
      type: "init";
      nodes: Array<{ id: string; x: number; y: number }>;
      links: Array<{ source: string; target: string }>;
      options: InitOptions;
    }
  | { type: "stop" }
  | { type: "kill" };

declare function postMessage(message: unknown, transfer: Transferable[]): void;
declare function postMessage(message: unknown): void;

let sim: Simulation<NodeDatum> | null = null;

self.onmessage = (event: MessageEvent<InMessage>) => {
  const msg = event.data;

  if (msg.type === "init") {
    if (sim) {
      sim.stop();
      sim = null;
    }

    const { nodes: rawNodes, links: rawLinks, options } = msg;

    const nodes: NodeDatum[] = rawNodes.map((n) => ({
      id: n.id,
      x: n.x,
      y: n.y,
    }));

    const links: LinkDatum[] = rawLinks.map((l) => ({
      source: l.source,
      target: l.target,
    }));

    const repulsionStrength = -options.repulsion * 30;
    const gravityStrength = options.gravity * 0.5;

    const simulation = forceSimulation<NodeDatum>(nodes, 2)
      .alphaDecay(0.05)
      .force(
        "link",
        forceLink<NodeDatum, LinkDatum>(links)
          .id((d) => d.id)
          .distance(30),
      )
      .force("charge", forceManyBody<NodeDatum>().strength(repulsionStrength))
      .force("center", forceCenter<NodeDatum>(0, 0).strength(gravityStrength))
      .stop(); // don't auto-start — we run manually

    sim = simulation;

    // Run simulation in batches, sending ~15 position updates for smooth settling animation
    const n = nodes.length;
    const TOTAL_TICKS = 120;
    const UPDATE_INTERVAL = 8;

    for (let t = 0; t < TOTAL_TICKS; t++) {
      simulation.tick(1);
      if ((t + 1) % UPDATE_INTERVAL === 0 || t === TOTAL_TICKS - 1) {
        const buf = new Float32Array(n * 2);
        for (let i = 0; i < n; i++) {
          buf[i * 2] = nodes[i].x ?? 0;
          buf[i * 2 + 1] = nodes[i].y ?? 0;
        }
        postMessage({ type: "tick", positions: buf }, [buf.buffer]);
      }
    }
    postMessage({ type: "end" });
    return;
  }

  if (msg.type === "stop") {
    if (sim) {
      sim.stop();
    }
    postMessage({ type: "end" });
    return;
  }

  if (msg.type === "kill") {
    if (sim) {
      sim.stop();
      sim = null;
    }
    self.close();
  }
};
