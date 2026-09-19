import { test, expect, type Page } from "@playwright/test";
import { normalizeBrainContext } from "../src/api/context";

async function select(page: Page, name: string) {
  await page.getByTestId("search-input").fill(name);
  await page.getByRole("listbox", { name: "Search results" })
    .getByRole("option", { name: new RegExp(name) }).first().click();
}

test("context decoder accepts documented omitted empty field and refuses absent populations", () => {
  const node = { uid: "sym:a", kind: "Symbol/Function", title: "A", location: "a.js:1", relevance: 1 };
  expect(normalizeBrainContext({ seeds: [node], connected: [] }).unresolved_seeds).toEqual([]);
  expect(() => normalizeBrainContext({ seeds: [node] })).toThrow("invalid connected nodes");
  expect(() => normalizeBrainContext({ connected: [] })).toThrow("invalid seeds");
  expect(() => normalizeBrainContext({ seeds: [node], connected: [], unresolved_seeds: null })).toThrow("invalid unresolved seeds");
});

test("release context returns stable real chain edges and never connects unrelated D", async ({ request }) => {
  const response = await request.post("/api/v1/brain/context", { data: { seeds: ["releaseA", "releaseD"] } });
  expect(response.ok()).toBeTruthy();
  const context = await response.json();
  const nodes = [...context.seeds, ...context.connected];
  const uid = (name: string) => nodes.find((node: { title: string }) => node.title === name)?.uid;
  for (const name of ["releaseA", "releaseB", "releaseC", "releaseD"]) expect(uid(name)).toBeTruthy();
  const pairs = context.edges.filter((edge: { edge_type: string }) => edge.edge_type === "CALLS")
    .map((edge: { source: string; target: string }) => [edge.source, edge.target]);
  expect(pairs).toContainEqual([uid("releaseA"), uid("releaseB")]);
  expect(pairs).toContainEqual([uid("releaseB"), uid("releaseC")]);
  expect(context.edges.some((edge: { source: string; target: string }) => edge.source === uid("releaseD") || edge.target === uid("releaseD"))).toBeFalsy();
  expect(context.graph_meta).toMatchObject({ edge_scope: "returned_nodes", edge_count_relation: "eq", omitted_edges: 0, truncated: false });
  const repeated = await request.post("/api/v1/brain/context", { data: { seeds: ["releaseA", "releaseD"] } });
  const again = await repeated.json();
  expect(again.edges).toEqual(context.edges);
  expect(again.graph_meta.generation).toBe(context.graph_meta.generation);
  const path = await request.get(`/api/v1/paths/${encodeURIComponent(uid("releaseA"))}/${encodeURIComponent(uid("releaseC"))}?max_depth=3`);
  expect(path.ok()).toBeTruthy();
  expect((await path.json()).map((item: { nodes: string[] }) => item.nodes))
    .toContainEqual([uid("releaseA"), uid("releaseB"), uid("releaseC")]);
  const trace = await request.get(`/api/v1/flow/${encodeURIComponent(uid("releaseA"))}?max_depth=3`);
  expect(trace.ok()).toBeTruthy();
  const flow = await trace.json();
  expect(flow.uid).toBe(uid("releaseA"));
  expect(flow.children[0].uid).toBe(uid("releaseB"));
  expect(flow.children[0].children[0].uid).toBe(uid("releaseC"));
  const impact = await request.get(`/api/v1/impact/${encodeURIComponent(uid("releaseC"))}?depth=3`);
  expect(impact.ok()).toBeTruthy();
  const impactBody = await impact.text();
  expect(impactBody).toContain(uid("releaseA"));
  expect(impactBody).toContain(uid("releaseB"));
  const oversized = await request.post("/api/v1/brain/context", { data: { seeds: Array(101).fill("releaseA") } });
  expect(oversized.status()).toBe(400);
});

test("release Local exposes the same CALLS in JSON, Table and Matrix", async ({ page }) => {
  await page.goto("/");
  await select(page, "releaseA");
  await page.getByRole("group", { name: "Graph mode" }).getByRole("button", { name: "Local", exact: true }).click();
  await expect(page.getByRole("status", { name: "Local result state" })).toContainText("stored relationships");
  const tabs = page.getByRole("tablist", { name: "Result representation" });
  await tabs.getByRole("tab", { name: "JSON representation", exact: true }).click();
  const payload = JSON.parse(await page.getByRole("region", { name: "JSON result view" }).locator("pre").innerText());
  expect(payload.graph.edges.filter((edge: { edgeType: string }) => edge.edgeType === "CALLS")).toHaveLength(2);
  await tabs.getByRole("tab", { name: "Table representation", exact: true }).click();
  await expect(page.getByRole("region", { name: "Node table view" }).getByText(/incoming, .*outgoing: CALLS/).first()).toBeVisible();
  await tabs.getByRole("tab", { name: "Matrix representation", exact: true }).click();
  await expect(page.getByRole("button", { name: /releaseA to releaseB: CALLS/ })).toBeVisible();
  await expect(page.getByRole("button", { name: /releaseB to releaseC: CALLS/ })).toBeVisible();
});

test("release Features has honest empty, loaded and failed states without stale graph", async ({ page }) => {
  await page.goto("/");
  const modes = page.getByRole("group", { name: "Graph mode" });
  await modes.getByRole("button", { name: "Features", exact: true }).click();
  await expect(page.getByRole("status", { name: "Features result state" })).toContainText("Select a node");
  await select(page, "releaseA");
  await modes.getByRole("button", { name: "Features", exact: true }).click();
  await expect(page.getByRole("status", { name: "Features result state" })).toContainText("stored relationships");
  await modes.getByRole("button", { name: "Overview", exact: true }).click();
  await page.route("**/api/v1/brain/context", (route) => route.fulfill({ status: 503, contentType: "application/json", body: JSON.stringify({ error: "Fixture context unavailable" }) }));
  await modes.getByRole("button", { name: "Features", exact: true }).click();
  await expect(page.getByRole("status", { name: "Features result state" })).toContainText("Fixture context unavailable");
  await page.getByRole("tablist", { name: "Result representation" }).getByRole("tab", { name: "JSON representation", exact: true }).click();
  const payload = JSON.parse(await page.getByRole("region", { name: "JSON result view" }).locator("pre").innerText());
  expect(payload.graph.nodes).toEqual([]);
  expect(payload.graph.edges).toEqual([]);
  await page.unroute("**/api/v1/brain/context");
  await page.getByRole("status", { name: "Features result state" }).getByRole("button", { name: "Retry" }).click();
  await expect(page.getByRole("status", { name: "Features result state" })).toContainText("stored relationships");
});
