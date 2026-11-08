import { expect, test } from "@playwright/test";
import type { BrainContextResult, BrainNode, ContextEdge } from "../src/api/types";
import { capBrainContextResult, DIFF_JSON_CAPS } from "../src/components/workspace/capBrainContextResult";

function node(uid: string): BrainNode {
  return { uid, kind: "Symbol/Function", title: `Readable ${uid}`, location: "fixture.js:1", relevance: 1 };
}

function edge(source: string, target: string): ContextEdge {
  return { source, target, edge_type: "CALLS", confidence: 1, evidence: "owned fixture" };
}

const publication = {
  generation: 73, node_limit: 9999, edge_limit: 9999, omitted_nodes: 7,
  omitted_edges: 11, edge_count_relation: "gte", truncated: true,
  edge_scope: "returned_nodes",
} as const;

test("snapshot JSON bounds edges after endpoint filtering and preserves historical facts (nw-945)", () => {
  const seeds = Array.from({ length: 101 }, (_, i) => node(`sym:seed:${i}`));
  const connected = Array.from({ length: 201 }, (_, i) => node(`sym:connected:${i}`));
  const excluded = [...Array.from({ length: 1000 }, () => edge("sym:absent", seeds[0].uid)),
    edge(seeds[100].uid, connected[0].uid), edge(seeds[0].uid, connected[200].uid), edge("sym:absent", seeds[0].uid)];
  const eligible = Array.from({ length: 1003 }, (_, i) => edge(seeds[i % 100].uid, connected[Math.floor(i / 100)].uid));
  const snapshot: BrainContextResult = {
    seeds, connected, edges: [...excluded, ...eligible], graph_meta: publication,
    unresolved_seeds: Array.from({ length: 101 }, (_, i) => `missing${i}`),
  };
  const original = JSON.stringify(snapshot);
  const result = capBrainContextResult(snapshot)!;
  expect(result.seeds).toHaveLength(100);
  expect(result.connected).toHaveLength(200);
  expect(result.unresolved_seeds).toHaveLength(100);
  expect(result._meta.seeds).toMatchObject({ total_count: 101, exported_count: 100, omitted_count: 1 });
  expect(result._meta.connected).toMatchObject({ total_count: 201, exported_count: 200, omitted_count: 1 });
  expect(result.edges).toEqual(eligible.slice(0, DIFF_JSON_CAPS.diffEdges));
  expect(result._meta.edges).toMatchObject({ total_count: 2006, eligible_count: 1003, exported_count: 1000,
    filtered_count: 1003, omitted_by_limit: 3, omitted_count: 1006, limit: 1000, truncated: true });
  const uids = new Set([...result.seeds, ...result.connected].map((item) => item.uid));
  expect(result.edges!.every((item) => uids.has(item.source) && uids.has(item.target))).toBe(true);
  expect(result.graph_meta).toEqual(publication);
  expect(JSON.stringify(snapshot)).toBe(original);
  expect(capBrainContextResult(snapshot)).toEqual(result);
});

for (const count of [999, 1000, 1001]) {
  test(`snapshot JSON edge cap boundary ${count} has exact accounting (nw-945)`, () => {
    const snapshot: BrainContextResult = { seeds: [node("sym:a")], connected: [], unresolved_seeds: [],
      edges: Array.from({ length: count }, () => edge("sym:a", "sym:a")) };
    const result = capBrainContextResult(snapshot)!;
    expect(result.edges).toHaveLength(Math.min(count, 1000));
    expect(result._meta.edges).toMatchObject({ total_count: count, filtered_count: 0,
      exported_count: Math.min(count, 1000), omitted_by_limit: Math.max(0, count - 1000),
      omitted_count: Math.max(0, count - 1000), truncated: count > 1000 });
  });
}

test("snapshot JSON separates client filtering from an uncapped server publication (nw-945)", () => {
  const snapshot: BrainContextResult = {
    seeds: [node("sym:canonical")], connected: [], unresolved_seeds: [],
    edges: [edge("sym:canonical", "canonical"), edge("sym:canonical", "sym:canonical")],
    graph_meta: { ...publication, omitted_nodes: 0, omitted_edges: 0, truncated: false, edge_count_relation: "eq" },
  };
  const result = capBrainContextResult(snapshot)!;
  expect(result.edges).toEqual([snapshot.edges![1]]);
  expect(result._meta.edges).toMatchObject({ filtered_count: 1, omitted_by_limit: 0, omitted_count: 1, truncated: true });
  expect(result.graph_meta).toEqual(snapshot.graph_meta);
});

test("snapshot JSON keeps legacy optional fields absent and empty fields empty (nw-945)", () => {
  const snapshot: BrainContextResult = { seeds: [node("sym:a")], connected: [], unresolved_seeds: [] };
  expect(capBrainContextResult(null)).toBeNull();
  const legacy = capBrainContextResult(snapshot)!;
  expect(legacy).not.toHaveProperty("edges");
  expect(legacy).not.toHaveProperty("graph_meta");
  expect(legacy._meta).not.toHaveProperty("edges");
  const empty = capBrainContextResult({ ...snapshot, edges: [] })!;
  expect(empty.edges).toEqual([]);
  expect(empty._meta.edges).toMatchObject({ total_count: 0, exported_count: 0, filtered_count: 0,
    omitted_by_limit: 0, omitted_count: 0, truncated: false });
  expect(empty).not.toHaveProperty("graph_meta");
  const unknown = JSON.parse(JSON.stringify(capBrainContextResult({ ...snapshot, graph_meta: {} } as BrainContextResult)));
  expect(unknown.graph_meta).toEqual({});
  expect(unknown.graph_meta).not.toHaveProperty("generation");
  const nullMeta = capBrainContextResult({ ...snapshot, graph_meta: null } as unknown as BrainContextResult)!;
  expect(nullMeta).not.toHaveProperty("graph_meta");
});

test("snapshot JSON does not widen the exported field whitelist (nw-945)", () => {
  const snapshot = {
    seeds: [node("sym:a")], connected: [], unresolved_seeds: [], private_debug: "snapshot-secret",
    edges: [{ ...edge("sym:a", "sym:a"), private_debug: "edge-secret" }],
    graph_meta: { ...publication, private_debug: { token: "publication-secret" } },
  };
  const result = capBrainContextResult(snapshot)!;
  expect(result).not.toHaveProperty("private_debug");
  expect(result.edges![0]).not.toHaveProperty("private_debug");
  expect(result.graph_meta).not.toHaveProperty("private_debug");
  expect(result.graph_meta).toEqual(publication);
});
