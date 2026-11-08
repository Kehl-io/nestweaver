import { readFile, writeFile } from "node:fs/promises";
import { expect, test, type APIRequestContext, type Page, type TestInfo } from "@playwright/test";
import type { BrainContextResult } from "../src/api/types";

async function screenshot(page: Page, info: TestInfo, name: string) {
  const path = info.outputPath(name);
  await page.screenshot({ path });
  await info.attach(name, { path, contentType: "image/png" });
}

async function jsonArtifact(info: TestInfo, name: string, value: unknown) {
  const path = info.outputPath(name);
  await writeFile(path, JSON.stringify(value, null, 2));
  await info.attach(name, { path, contentType: "application/json" });
}

async function releaseSymbol(request: APIRequestContext) {
  const response = await request.get("/api/v1/search?q=releaseA&limit=10");
  expect(response.ok()).toBeTruthy();
  const symbols = await response.json() as { uid: string; name: string; kind: string }[];
  const symbol = symbols.find((item) => item.name === "releaseA");
  expect(symbol).toBeTruthy();
  return symbol!;
}

async function selectRelease(page: Page, request: APIRequestContext) {
  const symbol = await releaseSymbol(request);
  await page.setViewportSize({ width: 1440, height: 900 });
  await page.goto(`/?node=${encodeURIComponent(symbol.uid)}&kind=${symbol.kind}`);
  await expect(page.getByTestId("detail-panel").getByText("releaseA").first()).toBeVisible();
  return symbol;
}

test("small backlog: metadata absence is settled, not a loading promise (nw-533)", async ({ page }) => {
  await page.goto("/?mode=repos&representation=table");
  await expect(page.getByRole("region", { name: "Result table" })).toBeVisible();
  await expect(page.getByText("Scene metadata will appear as results load.")).toHaveCount(0);
  await expect(page.getByText("No scene metadata yet")).toHaveCount(0);
});

test("small backlog: Context with no seeds offers scoped search (nw-594)", async ({ page }, testInfo) => {
  await page.goto("/?mode=context");
  const prompt = page.getByRole("region", { name: "Start context" });
  await expect(prompt).toContainText("Pick a symbol");
  await prompt.getByRole("button", { name: "Search symbols" }).click();
  await expect(page.getByTestId("search-input")).toBeFocused();
  await screenshot(page, testInfo, "empty-context-search.png");
});

test("small backlog: blank and same-node paths are explicit; large paste stays responsive (nw-592)", async ({ page, request }, testInfo) => {
  const symbol = await selectRelease(page, request);
  const requests: string[] = [];
  page.on("request", (req) => {
    if (new URL(req.url()).pathname.startsWith("/api/v1/paths/")) requests.push(req.url());
  });
  await page.getByTestId("detail-panel").getByRole("group", { name: "Node actions" }).first().getByRole("button", { name: "Path", exact: true }).click();
  const dialog = page.getByRole("dialog", { name: "Find path" });
  const target = dialog.getByRole("textbox", { name: "Path target" });
  await expect(dialog.getByRole("button", { name: "Find", exact: true })).toBeDisabled();
  await target.fill("   ");
  await target.press("Enter");
  await expect(dialog.getByRole("alert")).toContainText("Enter a target");
  const longTarget = `sym:${"Q".repeat(2000)}`;
  await target.fill(longTarget, { timeout: 5000 });
  await expect(target).toHaveValue(longTarget);
  await expect(target).toHaveAttribute("maxlength", "4096");
  await target.fill(symbol.uid);
  await target.press("Enter");
  await expect(page.getByRole("button", { name: "Path 1: 0 hops" }).first()).toBeVisible();
  expect(requests).toEqual([]);
  await screenshot(page, testInfo, "same-node-path.png");
});

for (const width of [1280, 1440, 1600, 1728]) {
  test(`small backlog: selected breadcrumb stays on one line at ${width}px (nw-854)`, async ({ page, request }) => {
    const symbol = await releaseSymbol(request);
    await page.setViewportSize({ width, height: 1000 });
    await page.goto(`/?node=${encodeURIComponent(symbol.uid)}&kind=${symbol.kind}`);
    const selected = page.getByRole("navigation", { name: "Scene breadcrumbs" }).getByTitle(symbol.uid, { exact: true });
    await expect(selected).toContainText(symbol.name);
    const bounds = await selected.evaluate((el) => ({ height: el.getBoundingClientRect().height, whitespace: getComputedStyle(el).whiteSpace, overflow: getComputedStyle(el).textOverflow }));
    expect(bounds.height).toBeLessThan(40);
    expect(bounds.whitespace).toBe("nowrap");
    expect(bounds.overflow).toBe("ellipsis");
  });
}

test("small backlog: Compare keeps snapshot names and UID access through layout changes (nw-866)", async ({ page, request }, testInfo) => {
  const replies: unknown[] = [];
  const contexts: { seeds: string[]; body: BrainContextResult }[] = [];
  const reads: Promise<unknown>[] = [];
  page.on("response", (response) => {
    if (new URL(response.url()).pathname === "/api/v1/brain/context") {
      reads.push(response.json().then((body) => {
        const input = response.request().postDataJSON();
        replies.push({ request: input, status: response.status(), body });
        if (response.ok() && input.token_budget === undefined) contexts.push({ seeds: input.seeds, body });
      }).catch((error) => replies.push({ error: String(error) })));
    }
  });
  const symbol = await selectRelease(page, request);
  await page.getByRole("button", { name: "Compare", exact: true }).first().click();
  const target = page.getByRole("textbox", { name: "Second context seeds" });
  await target.fill("releaseC");
  await target.press("Enter");
  const result = page.getByRole("region", { name: "Context comparison result" });
  await expect(result).toBeVisible();
  await expect(result.getByTitle(symbol.uid, { exact: true })).toHaveText("releaseA");
  await expect(result).toContainText("B: releaseC");
  await page.getByRole("tablist", { name: "Result representation" }).getByRole("tab", { name: "JSON representation", exact: true }).click();
  const json = page.getByRole("region", { name: "JSON result view" }).locator("pre");
  const before = JSON.parse(await json.innerText()).analysis.diff;
  await Promise.all(reads);
  // Compare snapshots retain real historical API evidence in every export
  // channel; layout stability alone cannot establish completeness (nw-945).
  for (const [key, seed, count] of [["snapshot_a", symbol.uid, 2], ["snapshot_b", "releaseC", 3]] as const) {
    const actual = contexts.find((reply) => reply.seeds.length === 1 && reply.seeds[0] === seed)?.body;
    expect(actual, `captured actual ${key} context`).toBeDefined();
    expect(actual!.edges).toHaveLength(count);
    expect(actual!.graph_meta).toMatchObject({ generation: expect.any(Number), edge_scope: "returned_nodes", edge_count_relation: "eq", truncated: false });
    expect(before[key].edges).toEqual(actual!.edges);
    expect(before[key].graph_meta).toEqual(actual!.graph_meta);
    expect(before[key]._meta.edges).toMatchObject({ total_count: count, exported_count: count,
      filtered_count: 0, omitted_by_limit: 0, omitted_count: 0, truncated: false });
  }
  await page.context().grantPermissions(["clipboard-read", "clipboard-write"]);
  const exports = page.getByRole("region", { name: "JSON result view" });
  await exports.getByRole("button", { name: "Copy", exact: true }).click();
  await expect(page.getByRole("region", { name: "Notifications", exact: true }).getByText("JSON copied", { exact: true })).toBeVisible();
  const copied = JSON.parse(await page.evaluate(() => navigator.clipboard.readText()));
  expect(copied.analysis.diff).toEqual(before);
  const downloadPromise = page.waitForEvent("download");
  await exports.getByRole("button", { name: "Save", exact: true }).click();
  const download = await downloadPromise;
  const savedPath = testInfo.outputPath("comparison-scene-saved.json");
  await download.saveAs(savedPath);
  const saved = JSON.parse(await readFile(savedPath, "utf8"));
  expect(saved.analysis.diff).toEqual(before);
  await testInfo.attach("comparison-scene-saved.json", { path: savedPath, contentType: "application/json" });
  await jsonArtifact(testInfo, "comparison-scene-copied.json", copied);
  await jsonArtifact(testInfo, "historical-comparison-before.json", before);
  await jsonArtifact(testInfo, "comparison-context-api.json", replies);
  // Layout settings belong to the Graph representation. Use its public
  // controls, then return to JSON to compare the exact historical payload.
  await page.getByRole("tablist", { name: "Result representation" }).getByRole("tab", { name: "Graph representation", exact: true }).click();
  await page.getByRole("button", { name: "Settings", exact: true }).click();
  await page.getByRole("slider", { name: "Force Scale" }).fill("4.5");
  await page.getByRole("tablist", { name: "Result representation" }).getByRole("tab", { name: "JSON representation", exact: true }).click();
  await expect(result.getByTitle(symbol.uid, { exact: true })).toHaveText("releaseA");
  expect(JSON.parse(await json.innerText()).analysis.diff).toEqual(before);
  await Promise.all(reads);
  await jsonArtifact(testInfo, "historical-comparison.json", before);
  await jsonArtifact(testInfo, "comparison-context-api.json", replies);
  await screenshot(page, testInfo, "comparison-layout.png");
});

test("small backlog: Result sorting reverses and symbol filter documents literal matching (nw-596)", async ({ page }) => {
  // One real fixture repository would make order reversal vacuous. Retain its
  // metadata and add two distinct display controls only for this table test.
  await page.route(/\/api\/v1\/repos(?:\?.*)?$/, async (route) => {
    const response = await route.fetch();
    const repos = await response.json();
    expect(repos.length).toBeGreaterThan(0);
    await route.fulfill({ response, json: [...repos,
      { ...repos[0], uid: "repo:sort-control:alpha", url: "file:///A-sort-control" },
      { ...repos[0], uid: "repo:sort-control:zeta", url: "file:///Z-sort-control" },
    ] });
  });
  await page.goto("/?mode=repos&representation=table");
  const table = page.getByRole("region", { name: "Result table" });
  const sort = table.getByRole("button", { name: /^Result/ });
  await expect(table.locator("tbody tr").first()).toBeVisible();
  await sort.click();
  await expect(table.getByRole("columnheader", { name: /^Result/ })).toHaveAttribute("aria-sort", "ascending");
  const ascending = await table.locator("[data-result-row-button='true']").allTextContents();
  expect(ascending.length).toBeGreaterThanOrEqual(3);
  await sort.click();
  await expect(table.getByRole("columnheader", { name: /^Result/ })).toHaveAttribute("aria-sort", "descending");
  expect(await table.locator("[data-result-row-button='true']").allTextContents()).toEqual([...ascending].reverse());
  await page.getByRole("tab", { name: "Symbols", exact: true }).click();
  const filter = page.getByRole("textbox", { name: "Filter symbols by name substring" });
  await expect(filter).toHaveAttribute("title", /regular expressions are not supported/);
  await filter.fill(".*");
  await expect(page.getByText("No symbols match the current filter.")).toBeVisible();
});

test("small backlog: folder activation expands without inventing a node selection (nw-596)", async ({ page }) => {
  await page.goto("/");
  await page.getByRole("tab", { name: "Files", exact: true }).click();
  const folder = page.getByRole("tree", { name: "Files", exact: true }).locator('[role="treeitem"][aria-expanded]').first();
  await expect(folder).toBeVisible();
  const before = await folder.getAttribute("aria-expanded");
  const url = page.url();
  await folder.click();
  await expect(folder).toHaveAttribute("aria-expanded", before === "true" ? "false" : "true");
  await expect(folder).toHaveAttribute("title", /(?:Expand|Collapse) folder/);
  expect(page.url()).toBe(url);
});

test("small backlog: selecting a folder scopes its subtree and can return to all files (nw-596)", async ({ page }, testInfo) => {
  await page.goto("/");
  await page.getByRole("tab", { name: "Files", exact: true }).click();
  const tree = page.getByRole("tree", { name: "Files", exact: true });
  const folder = tree.getByTitle("Show files in ui-folder; arrow keys expand or collapse", { exact: true });
  await expect(folder).toBeVisible();
  await folder.focus();
  const title = await folder.getAttribute("title");
  expect(title).toMatch(/^Show files in /);
  await folder.press("Enter");
  const scope = page.getByLabel("Selected folder");
  await expect(scope).toContainText("Files in ");
  const selected = tree.locator('[role="treeitem"][aria-level="1"]').first();
  await expect(selected).toHaveAttribute("aria-selected", "true");
  await expect(selected).toHaveAttribute("aria-setsize", "1");
  await expect(tree.locator('[role="treeitem"][aria-level="1"]')).toHaveCount(1);
  await expect(tree.getByTitle("ui-folder/entry.js", { exact: true })).toBeVisible();
  await expect(tree.getByTitle("ui-folder-other/entry.js", { exact: true })).toHaveCount(0);
  await selected.press("Space");
  await expect(selected).toHaveAttribute("aria-expanded", "false");
  await expect(scope).toContainText("Files in ui-folder");
  await expect(tree.getByTitle("ui-folder/entry.js", { exact: true })).toHaveCount(0);
  await selected.press("ArrowRight");
  await expect(tree.getByTitle("ui-folder/entry.js", { exact: true })).toBeVisible();
  await expect(tree.locator('[role="treeitem"][tabindex="0"]')).toHaveCount(1);
  await screenshot(page, testInfo, "selected-folder-subtree.png");
  await scope.getByRole("button", { name: "Show all files" }).click();
  await expect(scope).toHaveCount(0);
  await expect(tree.getByTitle(title!, { exact: true })).toBeFocused();
});

test("small backlog: real mixed-seed context retains resolved graph and discloses unknown seed (nw-942)", async ({ request }, testInfo) => {
  const missing = "__nw942_manual_missing_seed__";
  const response = await request.post("/api/v1/brain/context", { data: { seeds: ["releaseA", missing], token_budget: 6000 } });
  expect(response.ok()).toBeTruthy();
  const context = await response.json();
  await jsonArtifact(testInfo, "mixed-seed-real-context.json", context);
  expect(context.unresolved_seeds).toContain(missing);
  expect(context.seeds.some((node: { title: string }) => node.title === "releaseA")).toBe(true);
  const returned = new Set([...context.seeds, ...context.connected].map((node: { uid: string }) => node.uid));
  expect(context.edges.some((edge: { edge_type: string }) => edge.edge_type === "CALLS")).toBe(true);
  expect(context.edges.every((edge: { source: string; target: string }) => returned.has(edge.source) && returned.has(edge.target))).toBe(true);
  expect(context.graph_meta).toMatchObject({ edge_scope: "returned_nodes", edge_count_relation: "eq", truncated: false });
});

test("small backlog: Impact trust cannot overwrite a later Local lens (nw-569)", async ({ page, request }) => {
  await selectRelease(page, request);
  let arrived: () => void = () => undefined;
  let release: () => void = () => undefined;
  const captured = new Promise<void>((resolve) => { arrived = resolve; });
  const gate = new Promise<void>((resolve) => { release = resolve; });
  await page.route("**/api/v1/impact/**", async (route) => {
    const response = await route.fetch();
    arrived();
    await gate;
    await route.fulfill({ response }).catch(() => undefined);
  });
  try {
    await page.getByRole("group", { name: "Graph mode" }).getByRole("button", { name: "Impact", exact: true }).click();
    await captured;
    await page.getByRole("group", { name: "Graph mode" }).getByRole("button", { name: "Local", exact: true }).click();
    release();
    await expect(page.getByRole("navigation", { name: "Scene breadcrumbs" })).toContainText("Local");
    await expect(page.getByText(/impact is local-only and partial/i)).toHaveCount(0);
  } finally {
    release();
  }
});
