import type { BrainContextResult } from "../../api/types";

export const DIFF_JSON_CAPS = {
  diffSeeds: 100,
  diffConnected: 200,
  diffUnresolved: 100,
  diffEdges: 1000,
};

export function capList<T>(items: T[], limit: number) {
  const exported = items.slice(0, limit);
  return {
    items: exported,
    _meta: {
      total_count: items.length,
      exported_count: exported.length,
      limit,
      truncated: items.length > limit,
      omitted_count: Math.max(0, items.length - limit),
    },
  };
}

/** Bound the preview without changing the historical server publication facts. */
export function capBrainContextResult(snapshot: BrainContextResult | null) {
  if (!snapshot) return null;
  const seeds = capList(snapshot.seeds, DIFF_JSON_CAPS.diffSeeds);
  const connected = capList(snapshot.connected, DIFF_JSON_CAPS.diffConnected);
  const unresolved = capList(snapshot.unresolved_seeds, DIFF_JSON_CAPS.diffUnresolved);
  const retainedUids = new Set([...seeds.items, ...connected.items].map((node) => node.uid));
  const edges = [] as NonNullable<BrainContextResult["edges"]>;
  let eligibleEdges = 0;
  for (const edge of snapshot.edges ?? []) {
    if (!retainedUids.has(edge.source) || !retainedUids.has(edge.target)) continue;
    eligibleEdges += 1;
    if (edges.length < DIFF_JSON_CAPS.diffEdges) {
      edges.push({ source: edge.source, target: edge.target, edge_type: edge.edge_type,
        confidence: edge.confidence, evidence: edge.evidence });
    }
  }
  const edgeCount = snapshot.edges?.length ?? 0;
  const graphMeta = snapshot.graph_meta;
  return {
    seeds: seeds.items,
    connected: connected.items,
    unresolved_seeds: unresolved.items,
    ...(snapshot.edges === undefined ? {} : { edges }),
    // Keep the documented publication fields verbatim, without admitting
    // unknown/debug fields into a previously whitelisted export.
    ...(graphMeta == null ? {} : { graph_meta: {
      generation: graphMeta.generation,
      node_limit: graphMeta.node_limit,
      edge_limit: graphMeta.edge_limit,
      omitted_nodes: graphMeta.omitted_nodes,
      omitted_edges: graphMeta.omitted_edges,
      edge_count_relation: graphMeta.edge_count_relation,
      truncated: graphMeta.truncated,
      edge_scope: graphMeta.edge_scope,
    } }),
    _meta: {
      capped: true,
      seeds: seeds._meta,
      connected: connected._meta,
      unresolved_seeds: unresolved._meta,
      ...(snapshot.edges === undefined ? {} : { edges: {
        total_count: edgeCount,
        eligible_count: eligibleEdges,
        exported_count: edges.length,
        filtered_count: edgeCount - eligibleEdges,
        omitted_by_limit: eligibleEdges - edges.length,
        omitted_count: edgeCount - edges.length,
        limit: DIFF_JSON_CAPS.diffEdges,
        truncated: edgeCount > edges.length,
        filter_reason: "Only edges whose endpoints are in the exported seed/connected populations are included.",
      } }),
    },
  };
}
