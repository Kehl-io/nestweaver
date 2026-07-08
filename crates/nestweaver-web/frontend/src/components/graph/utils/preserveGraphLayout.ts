import type Graph from "graphology";

export interface GraphPosition {
  x: number;
  y: number;
}

export interface DeterministicPositionOptions {
  centerX?: number;
  centerY?: number;
  radius?: number;
}

export interface PreserveGraphLayoutOptions {
  xAttribute?: string;
  yAttribute?: string;
  fallbackRadius?: number;
  neighborDistance?: number;
  keepExistingNewNodePositions?: boolean;
}

const DEFAULT_FALLBACK_RADIUS = 120;
const DEFAULT_NEIGHBOR_DISTANCE = 36;
const FNV_OFFSET_BASIS = 2_166_136_261;
const FNV_PRIME = 16_777_619;

function hashString(value: string): number {
  let hash = FNV_OFFSET_BASIS;

  for (let i = 0; i < value.length; i++) {
    hash ^= value.charCodeAt(i);
    hash = Math.imul(hash, FNV_PRIME);
  }

  return hash >>> 0;
}

function unitHash(value: string): number {
  return hashString(value) / 0xffffffff;
}

function isFiniteNumber(value: unknown): value is number {
  return typeof value === "number" && Number.isFinite(value);
}

function readPosition(
  graph: Graph,
  nodeId: string,
  xAttribute: string,
  yAttribute: string,
): GraphPosition | null {
  const x = graph.getNodeAttribute(nodeId, xAttribute);
  const y = graph.getNodeAttribute(nodeId, yAttribute);

  if (!isFiniteNumber(x) || !isFiniteNumber(y)) return null;

  return { x, y };
}

function writePosition(
  graph: Graph,
  nodeId: string,
  position: GraphPosition,
  xAttribute: string,
  yAttribute: string,
): void {
  graph.setNodeAttribute(nodeId, xAttribute, position.x);
  graph.setNodeAttribute(nodeId, yAttribute, position.y);
}

function deterministicOffset(nodeId: string, distance: number): GraphPosition {
  const angle = unitHash(`${nodeId}:neighbor-angle`) * Math.PI * 2;
  const radius = distance * (0.75 + unitHash(`${nodeId}:neighbor-radius`) * 0.75);

  return {
    x: Math.cos(angle) * radius,
    y: Math.sin(angle) * radius,
  };
}

function knownNeighborCenter(
  graph: Graph,
  nodeId: string,
  knownPositions: Map<string, GraphPosition>,
): GraphPosition | null {
  let x = 0;
  let y = 0;
  let count = 0;

  for (const neighborId of graph.neighbors(nodeId)) {
    const position = knownPositions.get(neighborId);
    if (!position) continue;

    x += position.x;
    y += position.y;
    count += 1;
  }

  if (count === 0) return null;

  return {
    x: x / count,
    y: y / count,
  };
}

export function deterministicGraphPosition(
  nodeId: string,
  options: DeterministicPositionOptions = {},
): GraphPosition {
  const radius = options.radius ?? DEFAULT_FALLBACK_RADIUS;
  const angle = unitHash(`${nodeId}:angle`) * Math.PI * 2;
  const ring = radius * (0.35 + unitHash(`${nodeId}:ring`) * 0.9);

  return {
    x: (options.centerX ?? 0) + Math.cos(angle) * ring,
    y: (options.centerY ?? 0) + Math.sin(angle) * ring,
  };
}

export function preserveGraphLayout(
  graph: Graph,
  previousGraph: Graph | null | undefined,
  options: PreserveGraphLayoutOptions = {},
): Graph {
  const xAttribute = options.xAttribute ?? "x";
  const yAttribute = options.yAttribute ?? "y";
  const fallbackRadius = options.fallbackRadius ?? DEFAULT_FALLBACK_RADIUS;
  const neighborDistance = options.neighborDistance ?? DEFAULT_NEIGHBOR_DISTANCE;
  const knownPositions = new Map<string, GraphPosition>();

  if (previousGraph) {
    previousGraph.forEachNode((nodeId) => {
      if (!graph.hasNode(nodeId)) return;

      const position = readPosition(previousGraph, nodeId, xAttribute, yAttribute);
      if (!position) return;

      knownPositions.set(nodeId, position);
      writePosition(graph, nodeId, position, xAttribute, yAttribute);
    });
  }

  graph.forEachNode((nodeId) => {
    if (knownPositions.has(nodeId)) return;

    const currentPosition = readPosition(graph, nodeId, xAttribute, yAttribute);
    if (options.keepExistingNewNodePositions && currentPosition) {
      knownPositions.set(nodeId, currentPosition);
      return;
    }

    const neighborCenter = knownNeighborCenter(graph, nodeId, knownPositions);
    const offset = neighborCenter
      ? deterministicOffset(nodeId, neighborDistance)
      : null;
    const position = neighborCenter && offset
      ? {
          x: neighborCenter.x + offset.x,
          y: neighborCenter.y + offset.y,
        }
      : deterministicGraphPosition(nodeId, { radius: fallbackRadius });

    knownPositions.set(nodeId, position);
    writePosition(graph, nodeId, position, xAttribute, yAttribute);
  });

  return graph;
}
