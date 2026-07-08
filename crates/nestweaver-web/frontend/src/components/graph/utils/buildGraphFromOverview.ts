import Graph from "graphology";
import type { OverviewResponse, OverviewLandmark } from "../../../api/types";
import { kindToColor } from "./graphColors";
import { deterministicGraphPosition } from "./preserveGraphLayout";

const GOLDEN_ANGLE = Math.PI * (3 - Math.sqrt(5));
const HUB_SPACING = 46;
const MEMBER_MIN_DISTANCE = 24;
const MEMBER_DISTANCE_SPREAD = 20;

function landmarkPaletteKind(item: OverviewLandmark): string {
  if (item.kind === "repo") return "Section";
  if (item.kind === "service") return "Interface";
  if (item.kind === "symbol") return "Function";
  if (item.kind === "note") return "Note";
  return item.kind;
}

function landmarkColor(item: OverviewLandmark): string {
  return kindToColor(landmarkPaletteKind(item));
}

/** Extract the owning repo uid from service/symbol uids like
 * `svc:repo:<a>:<b>:<hash>` or `sym:repo:<a>:<b>:...`. */
export function parentRepoUid(uid: string): string | null {
  const match = /^(?:svc|sym):(repo:[^:]+:[^:]+)/.exec(uid);
  return match ? match[1] : null;
}

function hashUnit(value: string): number {
  let hash = 2_166_136_261;
  for (let i = 0; i < value.length; i++) {
    hash ^= value.charCodeAt(i);
    hash = Math.imul(hash, 16_777_619);
  }
  return (hash >>> 0) / 0xffffffff;
}

/** Repo hubs seed on a phyllotaxis spiral (sunflower pattern) with per-uid
 * jitter — organic scatter instead of a ring, stable across reloads. */
function hubSeedPosition(uid: string, index: number): { x: number; y: number } {
  const angle = index * GOLDEN_ANGLE + (hashUnit(`${uid}:hub-angle`) - 0.5) * 0.6;
  const radius = HUB_SPACING * Math.sqrt(index + 0.6) * (0.9 + hashUnit(`${uid}:hub-radius`) * 0.25);
  return { x: Math.cos(angle) * radius, y: Math.sin(angle) * radius };
}

function memberSeedPosition(
  uid: string,
  hub: { x: number; y: number },
): { x: number; y: number } {
  const angle = hashUnit(`${uid}:member-angle`) * Math.PI * 2;
  const distance = MEMBER_MIN_DISTANCE + hashUnit(`${uid}:member-radius`) * MEMBER_DISTANCE_SPREAD;
  return {
    x: hub.x + Math.cos(angle) * distance,
    y: hub.y + Math.sin(angle) * distance,
  };
}

const MAX_MEMBERS_PER_GALAXY = 14;

export function buildGraphFromOverview(result: OverviewResponse): Graph {
  const graph = new Graph({ type: "directed", multi: true });
  const maxScore = Math.max(...result.landmarks.map((n) => n.score), 0.001);

  const hubs = result.landmarks.filter((item) => item.kind === "repo");
  // Cap members per galaxy so one huge repo can't hairball the scene;
  // deterministic (score-then-uid ordered) so reloads stay stable
  const perGalaxy = new Map<string, number>();
  const members = result.landmarks
    .filter((item) => item.kind !== "repo")
    .sort((a, b) => (a.score === b.score ? a.uid.localeCompare(b.uid) : b.score - a.score))
    .filter((item) => {
      const parent = parentRepoUid(item.uid);
      if (!parent) return true;
      const count = perGalaxy.get(parent) ?? 0;
      if (count >= MAX_MEMBERS_PER_GALAXY) return false;
      perGalaxy.set(parent, count + 1);
      return true;
    });
  const hubPositions = new Map<string, { x: number; y: number }>();
  const memberCounts = new Map<string, number>();

  for (const item of members) {
    const parent = parentRepoUid(item.uid);
    if (parent) memberCounts.set(parent, (memberCounts.get(parent) ?? 0) + 1);
  }

  // Bigger galaxies (more members in scene) seed closer to the center
  const orderedHubs = [...hubs].sort(
    (left, right) =>
      (memberCounts.get(right.uid) ?? 0) - (memberCounts.get(left.uid) ?? 0),
  );

  const maxMemberCount = Math.max(...[...memberCounts.values()], 1);

  orderedHubs.forEach((item, index) => {
    const position = hubSeedPosition(item.uid, index);
    hubPositions.set(item.uid, position);
    const memberShare = (memberCounts.get(item.uid) ?? 0) / maxMemberCount;

    graph.addNode(item.uid, {
      label: item.label,
      x: position.x,
      y: position.y,
      size: 22 + memberShare * 14,
      color: landmarkColor(item),
      paletteKind: landmarkPaletteKind(item),
      kind: item.kind,
      location: item.location,
      // Raw overview scores sit at ~1.0 for every repo, which reads as
      // "everything is loud" downstream (emissive/bloom). Spread importance
      // by how much of the indexed scene each galaxy owns instead.
      relevance: 0.35 + memberShare * 0.65,
      reason: item.reason,
      forceLabel: true,
      isSeed: index < 8,
      isOverview: true,
    });
  });

  const maxHubRadius = HUB_SPACING * Math.sqrt(Math.max(orderedHubs.length, 1));

  members.forEach((item, index) => {
    const parent = parentRepoUid(item.uid);
    const hubPosition = parent ? hubPositions.get(parent) : undefined;
    const position = hubPosition
      ? memberSeedPosition(item.uid, hubPosition)
      : deterministicGraphPosition(item.uid, { radius: maxHubRadius + 70 });
    const normalized = Math.max(item.score / maxScore, 0.08);

    graph.addNode(item.uid, {
      label: item.label,
      x: position.x,
      y: position.y,
      size: 9 + normalized * 3,
      color: landmarkColor(item),
      paletteKind: landmarkPaletteKind(item),
      kind: item.kind,
      location: item.location,
      relevance: 0.15 + normalized * 0.25,
      reason: item.reason,
      forceLabel: false,
      isSeed: index < 2 && hubs.length === 0,
      isOverview: true,
    });

    if (parent && graph.hasNode(parent)) {
      graph.addEdge(parent, item.uid, {
        type: "overview",
        confidence: Math.max(item.score / maxScore, 0.18),
      });
    }
  });

  return graph;
}
