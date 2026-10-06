import { decodedReplyCount, eventCount, installDeliveryReceipts, renderedFrame, waitForDecodedReply } from "./deliveryBarriers";
import {
  expect,
  test,
  type APIRequestContext,
  type APIResponse,
  type Locator,
  type Page,
} from "@playwright/test";

interface SceneMetadata {
  workspace_id: string;
  workspace_type: string;
  trust: {
    data_scope: string;
    federation: string;
    freshness: string;
    result: string;
    unsupported: string[];
    message: string;
  };
  provenance: unknown[];
}

interface WorkspaceEntry {
  id: string;
  type: "all" | "project" | "repo" | "vault";
  label: string;
  uid?: string;
  _meta: SceneMetadata;
}

interface WorkspaceCatalogResponse {
  workspaces: WorkspaceEntry[];
  _meta: SceneMetadata;
}

interface SymbolCandidate {
  uid: string;
  name: string;
  kind: string;
  file_path: string;
  start_line: number;
}

interface SceneJsonPayload {
  _meta: SceneMetadata;
  active_lens: {
    lens: string;
    label: string;
    targetUid?: string | null;
    workspaceId?: string | null;
  };
  selected_node: {
    uid?: string | null;
    kind?: string | null;
  };
  representation: string;
  graph: {
    nodes: { uid: string; label: string }[];
    edges: unknown[];
    attributes?: {
      impact_states?: Record<string, unknown> | null;
      affected_tests?: Record<string, unknown> | null;
    };
  };
  analysis: {
    impact: {
      active: boolean;
      states?: Record<string, unknown> | null;
      affected_tests?: Record<string, unknown> | null;
    };
  };
}

async function waitForOk(
  requestCall: () => Promise<APIResponse>,
  label: string,
): Promise<APIResponse> {
  let lastStatus = 0;

  for (let attempt = 0; attempt < 40; attempt += 1) {
    const response = await requestCall();
    lastStatus = response.status();

    if (response.ok()) {
      return response;
    }

    await new Promise((resolve) => setTimeout(resolve, 250));
  }

  throw new Error(`${label} did not become ready; last status ${lastStatus}`);
}

async function getOk(
  request: APIRequestContext,
  path: string,
): Promise<APIResponse> {
  return waitForOk(() => request.get(path), path);
}

async function fetchWorkspaces(
  request: APIRequestContext,
): Promise<WorkspaceCatalogResponse> {
  const response = await getOk(request, "/api/v1/workspaces");
  const catalog = (await response.json()) as WorkspaceCatalogResponse;
  expect(Array.isArray(catalog.workspaces)).toBeTruthy();
  expect(catalog.workspaces.length).toBeGreaterThan(0);
  return catalog;
}

async function fetchFirstSymbol(
  request: APIRequestContext,
  query = "greet",
): Promise<SymbolCandidate> {
  const response = await getOk(
    request,
    `/api/v1/search?q=${encodeURIComponent(query)}&limit=8`,
  );
  const symbols = (await response.json()) as SymbolCandidate[];
  const symbol = symbols.find((candidate) =>
    candidate.name.toLowerCase().includes(query.toLowerCase()),
  ) ?? symbols[0];

  if (!symbol) {
    throw new Error(`fixture has no searchable symbol for ${query}`);
  }

  return symbol;
}

async function openP1Workspace(page: Page): Promise<void> {
  await page.setViewportSize({ width: 1440, height: 900 });
  await page.addInitScript(() => {
    window.localStorage.setItem(
      "nestweaver-ui",
      JSON.stringify({
        state: {
          layoutMode: "panels",
          representationMode: "graph",
          viewMode: "graph",
        },
        version: 6,
      }),
    );
  });
  await page.goto("/");
  await expect(page.getByTestId("graph-panel")).toBeVisible({ timeout: 15_000 });
  await expect(page.getByTestId("top-bar")).toBeVisible();
  await expect(page.getByTestId("status-bar")).toBeVisible();
  await expect(page.getByRole("tablist", { name: "Result representation" })).toBeVisible();
}

function escapeRegExp(value: string): string {
  return value.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
}

async function selectWorkspace(page: Page, workspace: WorkspaceEntry): Promise<void> {
  await page.getByLabel("Workspace").click();
  await page
    .getByRole("option", { name: new RegExp(escapeRegExp(workspace.label)) })
    .click();
  await expect(page.getByTestId("status-bar")).toContainText(workspace.label);
}

async function selectRepresentation(
  page: Page,
  label: "Graph" | "Table" | "JSON",
): Promise<void> {
  await page.getByRole("tab", { name: label }).click();
}

async function expectUrlParam(
  page: Page,
  name: string,
  value: string,
): Promise<void> {
  await expect
    .poll(
      () => new URL(page.url()).searchParams.get(name),
      { message: `URL should include ${name}=${value}` },
    )
    .toBe(value);
}

async function fillSearch(page: Page, phrase: string): Promise<Locator> {
  const searchInput = page.getByTestId("search-input");
  await searchInput.fill("");
  await searchInput.fill(phrase);
  const results = page.getByRole("listbox", { name: "Search results" });
  await expect(results).toBeVisible();
  return results;
}

function jsonResultRegion(page: Page): Locator {
  return page.getByRole("region", { name: "JSON result", exact: true });
}

async function jsonPayload(page: Page): Promise<SceneJsonPayload> {
  const code = jsonResultRegion(page).locator("code");
  const text = await code.textContent();
  if (!text) throw new Error("JSON result view did not render a payload");
  return JSON.parse(text) as SceneJsonPayload;
}

async function waitForJsonPayload(
  page: Page,
  predicate: (payload: SceneJsonPayload) => boolean,
  message: string,
): Promise<SceneJsonPayload> {
  await expect
    .poll(
      async () => {
        try {
          return predicate(await jsonPayload(page));
        } catch {
          return false;
        }
      },
      { message, timeout: 15_000 },
    )
    .toBe(true);

  return jsonPayload(page);
}

test.describe("P1 core workspace release gates", () => {
  test("selects a workspace and restores workspace plus representation deep links", async ({
    page,
    request,
  }) => {
    const catalog = await fetchWorkspaces(request);
    const repoWorkspace = catalog.workspaces.find((workspace) => workspace.type === "repo");

    if (!repoWorkspace) {
      test.skip(true, "fixture does not expose a repo workspace");
      return;
    }

    await openP1Workspace(page);
    await selectWorkspace(page, repoWorkspace);

    await selectRepresentation(page, "JSON");
    await expect(jsonResultRegion(page)).toBeVisible();
    await expectUrlParam(page, "workspace", repoWorkspace.id);
    await expectUrlParam(page, "representation", "json");

    await page.reload();
    await expect(page.getByTestId("graph-panel")).toBeVisible({ timeout: 15_000 });
    await expect(page.getByTestId("status-bar")).toContainText(repoWorkspace.label);
    await expect(jsonResultRegion(page)).toBeVisible();

    const payload = await waitForJsonPayload(
      page,
      (current) =>
        current._meta.workspace_id === repoWorkspace.id &&
        current.representation === "json" &&
        current._meta.trust != null &&
        current._meta.trust.result !== "loading" &&
        current.graph.nodes.length > 0,
      "JSON deep link should restore workspace and representation",
    );
    expect(payload._meta.trust).toHaveProperty("federation");
    expect(payload._meta.provenance.length).toBeGreaterThan(0);
  });

  test("previews expensive Search Phrases, recovers ambiguity, and exposes unsupported phrases", async ({
    page,
  }) => {
    await openP1Workspace(page);

    let results = await fillSearch(page, "impact of greet");
    await expect(results.getByText("impact of <symbol>")).toBeVisible();
    await expect(results.getByText("preview", { exact: true })).toBeVisible();
    await expect(results.getByRole("button", { name: "Run" })).toBeVisible();

    await page.route("**/api/v1/search?q=ambiguous&limit=*", async (route) => {
      await route.fulfill({
        contentType: "application/json",
        body: JSON.stringify([
          {
            uid: "sym:testdata/js/simple.js:ambiguous_one",
            name: "ambiguous",
            kind: "Function",
            file_path: "testdata/js/simple.js",
            start_line: 5,
          },
          {
            uid: "sym:testdata/js/other.js:ambiguous_two",
            name: "ambiguous",
            kind: "Function",
            file_path: "testdata/js/other.js",
            start_line: 9,
          },
        ]),
      });
    });
    await page.route("**/api/v1/brain/search?q=ambiguous&limit=*", async (route) => {
      await route.fulfill({
        contentType: "application/json",
        body: JSON.stringify([]),
      });
    });

    results = await fillSearch(page, "impact of ambiguous");
    await expect(results.getByText("Choose target: ambiguous")).toBeVisible();
    await expect(results.getByText("testdata/js/simple.js")).toBeVisible();
    await expect(results.getByText("testdata/js/other.js")).toBeVisible();

    results = await fillSearch(page, "contract drift");
    await expect(results.getByText("Contract drift", { exact: true })).toBeVisible();
    await expect(results.getByText("Unsupported", { exact: true })).toBeVisible();
    await expect(
      results.getByText("Contract drift is not wired to a P1 web route yet."),
    ).toBeVisible();
    await expect(results.getByRole("button", { name: "Unavailable" })).toBeDisabled();
  });

  test("knowledge cards expose identity, evidence, trust, relationships, and action parity", async ({
    page,
    request,
  }) => {
    const symbol = await fetchFirstSymbol(request);

    await openP1Workspace(page);
    const results = await fillSearch(page, symbol.name);
    const option = results
      .getByRole("option", { name: new RegExp(escapeRegExp(symbol.name)) })
      .first();
    await option.getByRole("button", { name: "Detail" }).click();

    const detailPanel = page.getByTestId("detail-panel");
    await expect(detailPanel).toBeVisible();
    await detailPanel
      .getByRole("button", { name: /Open source|Open detail/ })
      .first()
      .click();

    const knowledgeCard = page.locator("article").filter({ hasText: symbol.name }).last();
    await expect(knowledgeCard).toBeVisible();
    await expect(knowledgeCard.getByRole("heading", { name: "Role", exact: true })).toBeVisible();
    await expect(knowledgeCard.getByRole("heading", { name: "Evidence", exact: true })).toBeVisible();
    await expect(knowledgeCard.getByRole("heading", { name: "Relationships", exact: true })).toBeVisible();
    await expect(knowledgeCard.getByText(/Ready|Loading|Limited/).first()).toBeVisible();
    await expect(knowledgeCard.getByText(/local-only|federated|unknown/).first()).toBeVisible();

    const actions = knowledgeCard.locator('[aria-label="Node actions"]');
    for (const action of [
      "Explore",
      "Impact",
      "Trace",
      "Path",
      "Ask",
      /Open source|Open detail/,
      "Copy link",
    ]) {
      await expect(actions.getByRole("button", { name: action }).first()).toBeVisible();
    }
  });

  test("switches core result sets between graph, table, and JSON representations", async ({
    page,
  }) => {
    await openP1Workspace(page);

    await expect(
      page.getByRole("application", { name: "Code knowledge graph" }),
    ).toBeVisible();

    await selectRepresentation(page, "Table");
    await expect(page.getByRole("region", { name: "Result table" })).toBeVisible();
    await expect(page.getByRole("table")).toBeVisible();

    await selectRepresentation(page, "JSON");
    await expect(jsonResultRegion(page)).toBeVisible();
    const payload = await jsonPayload(page);
    expect(payload).toHaveProperty("_meta");
    expect(payload).toHaveProperty("graph");
    expect(payload._meta.trust).toHaveProperty("data_scope");

    await selectRepresentation(page, "Graph");
    await expect(
      page.getByRole("application", { name: "Code knowledge graph" }),
    ).toBeVisible();
  });

  test("opens Impact with trust metadata and restores an Impact deep link", async ({
    page,
    request,
  }) => {
    const symbol = await fetchFirstSymbol(request);

    await openP1Workspace(page);
    const results = await fillSearch(page, `impact of ${symbol.name}`);
    await results.getByRole("button", { name: "Run" }).click();

    await expectUrlParam(page, "mode", "impact");
    await expectUrlParam(page, "lens", "impact");
    await expectUrlParam(page, "node", symbol.uid);

    await selectRepresentation(page, "JSON");
    await expectUrlParam(page, "representation", "json");

    const payload = await waitForJsonPayload(
      page,
      (current) =>
        current.active_lens.lens === "impact" &&
        current.selected_node.uid === symbol.uid &&
        current.analysis.impact.active,
      "Impact JSON should expose active lens and analysis metadata",
    );
    expect(payload.analysis.impact.states).toBeTruthy();
    expect(payload.graph.attributes?.impact_states).toBeTruthy();
    expect(payload._meta.trust).toHaveProperty("result");

    await page.reload();
    await expect(page.getByTestId("graph-panel")).toBeVisible({ timeout: 15_000 });
    await expect(jsonResultRegion(page)).toBeVisible();
    const restored = await waitForJsonPayload(
      page,
      (current) =>
        current.active_lens.lens === "impact" &&
        current.selected_node.uid === symbol.uid &&
        current.representation === "json" &&
        Boolean(current.analysis.impact.states),
      "Impact deep link should restore lens, node, JSON representation, and metadata",
    );
    expect(restored.analysis.impact.states).toBeTruthy();
  });
});

// Scoped replies differ deliberately: a global fetch cannot impersonate a scoped Files tree.
function deliveryWorkspace(id: string, type: WorkspaceEntry["type"], label: string) {
  const counts = { project_count: 0, repo_count: type === "vault" ? 0 : 2, service_count: 0,
    vault_count: type === "vault" ? 1 : 0, note_count: type === "vault" ? 1 : 0, symbol_count: 1 };
  const _meta = { workspace_id: id, workspace_type: type,
    trust: { data_scope: "local-only", federation: "local-only", freshness: "current", capability: "local-index",
      result: "complete", source_confidence: "extracted", partial: false, unsupported: [], message: "" },
    provenance: [{ source: "fixture", detail: "scoped fixture" }], truncation: { truncated: false }, continuation: { has_more: false } };
  return { id, type, label, uid: id, counts, _meta };
}

async function deliveryEvents(page: Page) {
  const events = { pending: "" };
  await page.route("**/api/v1/events", (route) => {
    const body = `retry: 200\n${events.pending}\n`;
    events.pending = "";
    return route.fulfill({ status: 200, contentType: "text/event-stream", body });
  });
  return events;
}

test("Files scopes repos before symbol limits and rejects an obsolete workspace reply", async ({ page }) => {
  await installDeliveryReceipts(page);
  const events = await deliveryEvents(page);
  let filesCommitted = false;
  const workspaces = [deliveryWorkspace("all", "all", "All indexed content"),
    deliveryWorkspace("project:files", "project", "Files project"),
    deliveryWorkspace("repo:beta", "repo", "Beta repository"),
    deliveryWorkspace("vault:files", "vault", "Files notes")];
  await page.route("**/api/v1/workspaces", (route) => route.fulfill({ json: { workspaces, _meta: workspaces[0]._meta } }));
  const repo = (uid: string) => ({ uid, url: `https://fixture/${uid.slice(5)}.git`, indexed_sha: "fixture",
    staleness_commits_behind: 0, instance_id: "fixture", name: null, root_path: null });
  let releaseProject: (() => void) | undefined;
  let holdProject = false;
  let projectPending = false;
  let obsoleteDelivered = false;
  await page.route(/\/api\/v1\/repos(?:\?|$)/, async (route) => {
    const scope = new URL(route.request().url()).searchParams.get("workspace") ?? "all";
    if (scope === "project:files" && holdProject) {
      projectPending = true;
      await new Promise<void>((resolve) => { releaseProject = resolve; });
    }
    const rows = scope === "vault:files" ? [] : scope === "repo:beta" ? [repo("repo:beta")]
      : scope === "project:files" ? [repo("repo:alpha"), repo("repo:empty")] : [repo("repo:alpha"), repo("repo:beta"), repo("repo:empty")];
    await route.fulfill({ json: rows });
    if (scope === "project:files" && holdProject) obsoleteDelivered = true;
  });
  await page.route("**/api/v1/symbols/top?**", (route) => {
    const scope = new URL(route.request().url()).searchParams.get("workspace") ?? "all";
    const uid = scope === "repo:beta" ? "repo:beta" : "repo:alpha";
    return route.fulfill({ json: scope === "vault:files" ? [] : [{ uid: `sym:${uid}:one`, repo_uid: uid,
      name: "one", kind: "Function", file_path: `${uid === "repo:beta" ? "folder/nested/" : ""}${uid.slice(5)}${filesCommitted ? "-committed" : ""}.ts`, start_line: 1 }] });
  });
  await openP1Workspace(page);
  const explorer = page.getByTestId("explorer-panel");
  await explorer.getByRole("tab", { name: "Files", exact: true }).click();
  const tree = explorer.getByRole("tree", { name: "Files" });
  await expect(tree).toContainText("beta.git");
  await selectWorkspace(page, workspaces[1]);
  await expect(tree).toContainText("alpha.ts");
  await expect(tree).toContainText("empty.git"); // membership does not come from returned symbols
  await expect(tree).not.toContainText("beta.git");
  await selectWorkspace(page, workspaces[2]);
  await tree.getByText("nested", { exact: true }).click();
  await expect(tree.getByText("beta.ts", { exact: true })).toBeVisible();
  await expect(tree).not.toContainText("alpha.git");
  filesCommitted = true;
  events.pending = "event: graph:updated\ndata: {}\n\n";
  await expect(tree.getByText("beta-committed.ts", { exact: true })).toBeVisible();
  await expect(tree).not.toContainText("beta.ts");
  holdProject = true;
  await selectWorkspace(page, workspaces[1]);
  await expect.poll(() => projectPending).toBe(true);
  await selectWorkspace(page, workspaces[2]);
  await tree.getByText("nested", { exact: true }).click();
  await expect(tree.getByText("beta-committed.ts", { exact: true })).toBeVisible();
  const beforeRelease = await decodedReplyCount(page, "/api/v1/repos", { workspace: "project:files" });
  releaseProject!();
  await expect.poll(() => obsoleteDelivered).toBe(true);
  await waitForDecodedReply(page, "/api/v1/repos", { workspace: "project:files" }, beforeRelease);
  await expect(tree).not.toContainText("alpha.git");
  await selectWorkspace(page, workspaces[3]);
  await expect(explorer).toContainText(/No (repos|code)/i);
  await expect(tree).toHaveCount(0);
  await selectWorkspace(page, workspaces[0]);
  await expect(tree).toContainText("alpha.git");
  await expect(tree).toContainText("beta.git");
  await expect(tree).toContainText("empty.git");
});

test("committed event bursts refresh held catalogs and overview once, heartbeat stays quiet", async ({ page }) => {
  await installDeliveryReceipts(page);
  await page.clock.install();
  const events = await deliveryEvents(page);
  const workspace = deliveryWorkspace("all", "all", "Held catalog before");
  let committed = false;
  let catalogs = 0;
  let overviews = 0;
  await page.route("**/api/v1/workspaces", (route) => {
    catalogs += 1;
    return route.fulfill({ json: { workspaces: [{ ...workspace, label: committed ? "Held catalog after" : workspace.label }], _meta: workspace._meta } });
  });
  await page.route("**/api/v1/overview?**", (route) => {
    overviews += 1;
    const landmark = { uid: "note:held", kind: "note", label: committed ? "Committed note" : "Previous note",
      location: "held.md", score: 1, reason: "fixture" };
    return route.fulfill({ json: { counts: { ...workspace.counts, gap_count: 0 }, landmarks: [landmark],
      start_here: [landmark], gaps: [], _meta: workspace._meta } });
  });
  await openP1Workspace(page);
  await selectRepresentation(page, "JSON");
  await expect(jsonResultRegion(page)).toContainText("Previous note");
  const baseline = { catalogs, overviews };
  const heartbeats = await eventCount(page, "watcher:status");
  events.pending = "event: watcher:status\ndata: {}\n\n";
  await expect.poll(() => eventCount(page, "watcher:status")).toBeGreaterThan(heartbeats);
  await page.clock.runFor(401); // exhaust the actual 400 ms coalescing timer
  await renderedFrame(page);
  expect({ catalogs, overviews }).toEqual(baseline);
  committed = true;
  const updates = await eventCount(page, "graph:updated");
  const refreshes = await eventCount(page, "full_refresh");
  events.pending = "event: graph:updated\ndata: {}\n\nevent: full_refresh\ndata: {}\n\nevent: graph:updated\ndata: {}\n\n";
  await expect.poll(() => eventCount(page, "graph:updated")).toBe(updates + 2);
  await expect.poll(() => eventCount(page, "full_refresh")).toBe(refreshes + 1);
  await page.clock.runFor(401);
  await expect(page.getByLabel("Workspace", { exact: true })).toContainText("Held catalog after");
  await expect(jsonResultRegion(page)).toContainText("Committed note");
  await page.clock.runFor(401);
  await renderedFrame(page);
  expect(catalogs).toBe(baseline.catalogs + 1);
  expect(overviews).toBe(baseline.overviews + 1);
  await expectUrlParam(page, "representation", "json");
  expect((await jsonPayload(page)).active_lens.lens).toBe("overview");
});

test("context refresh failure retains the scene and announces the retryable API message", async ({ page, request }) => {
  const symbol = await fetchFirstSymbol(request);
  const events = await deliveryEvents(page);
  let fail = false;
  await page.route("**/api/v1/brain/context", (route) => fail
    ? route.fulfill({ status: 503, json: { error: "route_capability", message: "Context is temporarily unavailable; retry with CLI brain context.", semantic_applied: false } })
    : route.continue());
  await openP1Workspace(page);
  const results = await fillSearch(page, symbol.name);
  await results.getByRole("option").first().getByRole("button", { name: "Explore" }).click();
  await selectRepresentation(page, "JSON");
  await waitForJsonPayload(page, (p) => p.active_lens.lens === "context" && p.graph.nodes.some((n) => n.uid === symbol.uid), "context loaded");
  const before = await jsonPayload(page);
  fail = true;
  events.pending = "event: graph:updated\ndata: {}\n\n";
  await expect(page.getByRole("region", { name: "Notifications", exact: true }).getByText("Context is temporarily unavailable; retry with CLI brain context.", { exact: true })).toBeVisible();
  const after = await jsonPayload(page);
  expect(after.graph.nodes.map((n) => n.uid).sort()).toEqual(before.graph.nodes.map((n) => n.uid).sort());
  expect(after.graph.edges).toEqual(before.graph.edges);
  await expectUrlParam(page, "node", symbol.uid);
});

test("workspace history clears the previous scope while the new overview is pending and fails", async ({ page }) => {
  const workspaces = [deliveryWorkspace("all", "all", "Navigation scope A"),
    deliveryWorkspace("repo:nav", "repo", "Navigation scope B")];
  await page.route("**/api/v1/workspaces", (route) => route.fulfill({ json: { workspaces, _meta: workspaces[0]._meta } }));
  let failScopeA = false;
  let scopeAPending = false;
  let failureDelivered = false;
  let releaseScopeA: (() => void) | undefined;
  await page.route("**/api/v1/overview?**", async (route) => {
    const scope = new URL(route.request().url()).searchParams.get("workspace") ?? "all";
    if (scope === "all" && failScopeA) {
      scopeAPending = true;
      await new Promise<void>((resolve) => { releaseScopeA = resolve; });
      await route.fulfill({ status: 503, json: { error: "route_capability", message: "Scope A overview is temporarily unavailable. Retry." } });
      failureDelivered = true;
      return;
    }
    const workspace = scope === "all" ? workspaces[0] : workspaces[1];
    const landmark = { uid: scope === "all" ? "note:nav-a" : "note:nav-b", kind: "note",
      label: workspace.label, location: "navigation.md", score: 1, reason: "scoped fixture" };
    await route.fulfill({ json: { counts: { ...workspace.counts, gap_count: 0 }, landmarks: [landmark],
      start_here: [landmark], gaps: [], _meta: workspace._meta } });
  });
  await openP1Workspace(page);
  await selectRepresentation(page, "JSON");
  await waitForJsonPayload(page, (p) => p.graph.nodes.some((n) => n.uid === "note:nav-a"), "scope A rendered a nonempty overview");
  await selectWorkspace(page, workspaces[1]);
  await waitForJsonPayload(page, (p) => p.graph.nodes.some((n) => n.uid === "note:nav-b") && p._meta.workspace_id === "repo:nav", "scope B rendered its own overview");

  failScopeA = true;
  // Lens updates may also create history entries; follow history until the workspace changes.
  for (let step = 0; step < 6 && !scopeAPending; step += 1) {
    await page.getByRole("button", { name: "Undo scene navigation", exact: true }).click();
    await renderedFrame(page);
  }
  await expect.poll(() => scopeAPending, { message: "history requested the previous workspace overview" }).toBe(true);
  await expect(page.getByLabel("Workspace", { exact: true })).toContainText(workspaces[0].label);
  await selectRepresentation(page, "JSON");
  const pending = await jsonPayload(page);
  releaseScopeA!();
  await expect.poll(() => failureDelivered).toBe(true);
  await expect(jsonResultRegion(page)).toContainText("Scope A overview is temporarily unavailable. Retry.");
  const failed = await jsonPayload(page);
  expect(pending.graph.nodes.map((n) => n.uid)).not.toContain("note:nav-b");
  expect(failed.graph.nodes.map((n) => n.uid)).not.toContain("note:nav-b");
  expect(failed._meta.workspace_id).toBe("all");
});

test("Features with no seeds clears a rendered overview instead of relabeling its nodes", async ({ page }) => {
  const workspace = deliveryWorkspace("all", "all", "Features origin scope");
  await page.route("**/api/v1/workspaces", (route) => route.fulfill({ json: { workspaces: [workspace], _meta: workspace._meta } }));
  await page.route("**/api/v1/overview?**", (route) => {
    const landmark = { uid: "note:features-origin", kind: "note", label: "Overview-only landmark",
      location: "origin.md", score: 1, reason: "fixture" };
    return route.fulfill({ json: { counts: { ...workspace.counts, gap_count: 0 }, landmarks: [landmark],
      start_here: [landmark], gaps: [], _meta: workspace._meta } });
  });
  await openP1Workspace(page);
  await selectRepresentation(page, "JSON");
  await waitForJsonPayload(page, (p) => p.graph.nodes.some((n) => n.uid === "note:features-origin"), "overview rendered before changing mode");
  await page.getByRole("group", { name: "Graph mode", exact: true }).getByRole("button", { name: "Features", exact: true }).click();
  await expect(page.getByRole("status", { name: "Features result state", exact: true })).toContainText("Add a context seed to explore its stored relationships.");
  await expectUrlParam(page, "mode", "features");
  const emptyFeatures = await jsonPayload(page);
  expect(emptyFeatures.active_lens.label).toBe("Features");
  expect(emptyFeatures.graph.nodes).toEqual([]);
  expect(emptyFeatures.graph.edges).toEqual([]);
});


test("generation handshake catches a missed publish without reheating unchanged reconnects", async ({ page }) => {
  let generation = "1";
  let connections = 0;
  let catalogs = 0;
  const workspace = deliveryWorkspace("all", "all", "Generation fixture");
  await page.route("**/api/v1/events", (route) => {
    connections += 1;
    return route.fulfill({ status: 200, contentType: "text/event-stream",
      body: `retry: 200\nevent: graph:generation\ndata: ${JSON.stringify({ graph_generation: generation, pagerank_generation: "0" })}\n\n` });
  });
  await page.route("**/api/v1/workspaces", (route) => {
    catalogs += 1;
    return route.fulfill({ json: { workspaces: [workspace], _meta: workspace._meta } });
  });
  await openP1Workspace(page);
  await expect.poll(() => catalogs).toBeGreaterThan(1);
  await expect.poll(() => connections).toBeGreaterThan(3);
  const before = catalogs;
  const opens = connections;
  await expect.poll(() => connections).toBeGreaterThan(opens + 2);
  expect(catalogs).toBe(before);
  generation = "2"; // The missed commit emits no subsequent graph:updated event.
  await expect.poll(() => catalogs).toBe(before + 1);
  const caughtUp = connections;
  await expect.poll(() => connections).toBeGreaterThan(caughtUp + 2);
  expect(catalogs).toBe(before + 1);
});

test("Symbols refresh retains focused filter while the replacement request is pending", async ({ page }) => {
  const events = await deliveryEvents(page);
  let hold = false;
  let pending = false;
  let release: (() => void) | undefined;
  await page.route("**/api/v1/symbols/top?**", async (route) => {
    if (hold) { pending = true; await new Promise<void>((resolve) => { release = resolve; }); }
    await route.fulfill({ json: [{ uid: "sym:refresh", repo_uid: "repo:refresh", name: "RefreshWitness",
      kind: "Function", file_path: "refresh.ts", start_line: 1 }] });
  });
  await openP1Workspace(page);
  const explorer = page.getByTestId("explorer-panel");
  await explorer.getByRole("tab", { name: "Symbols", exact: true }).click();
  const filter = explorer.getByPlaceholder("Filter symbols...");
  await filter.fill("Witness");
  hold = true;
  events.pending = "event: graph:updated\ndata: {}\n\n";
  await expect.poll(() => pending).toBe(true);
  await expect(filter).toBeFocused();
  await expect(filter).toHaveValue("Witness");
  await expect(explorer.getByText("RefreshWitness", { exact: true })).toBeVisible();
  release!();
  await expect(filter).toBeFocused();
});

test("same overview resumes an interrupted worker but leaves a settled worker quiet", async ({ page }) => {
  await page.addInitScript(() => {
    const NativeWorker = window.Worker;
    const probe = { starts: 0, ends: [] as (() => void)[], inits: [] as { id: string; x: number; y: number }[][] };
    Object.assign(window, { layoutProbe: probe });
    window.Worker = class extends NativeWorker {
      constructor(url: string | URL, options?: WorkerOptions) {
        super(url, options);
        if (String(url).includes("forceLayoutWorker")) {
          const original = this.postMessage.bind(this);
          this.postMessage = (message: unknown) => {
            if ((message as { type?: string }).type === "init") {
              probe.starts += 1;
              const nodes = (message as { nodes: { id: string; x: number; y: number }[] }).nodes;
              probe.inits.push(nodes);
              if (probe.starts === 1) this.onmessage?.(new MessageEvent("message", {
                data: { type: "tick", positions: new Float32Array(nodes.flatMap((_, i) => [1200 + i, 2300 + i])) },
              }));
              probe.ends.push(() => this.onmessage?.(new MessageEvent("message", { data: { type: "end" } })));
            } else original(message);
          };
        }
      }
    };
  });
  const events = await deliveryEvents(page);
  const workspace = deliveryWorkspace("all", "all", "Worker fixture");
  let label = "Worker scene before";
  await page.route("**/api/v1/overview?**", (route) => {
    const landmark = { uid: "note:worker", kind: "note", label, location: "worker.md", score: 1, reason: "fixture" };
    return route.fulfill({ json: { counts: { ...workspace.counts, gap_count: 0 }, landmarks: [landmark], start_here: [landmark], gaps: [], _meta: workspace._meta } });
  });
  await openP1Workspace(page);
  await selectRepresentation(page, "JSON");
  const starts = () => page.evaluate(() => (window as unknown as { layoutProbe: { starts: number } }).layoutProbe.starts);
  await expect.poll(starts).toBeGreaterThan(0);
  const initial = await starts();
  label = "Interrupted refresh applied";
  events.pending = "event: graph:updated\ndata: {}\n\n";
  await expect(jsonResultRegion(page)).toContainText(label);
  await expect.poll(starts).toBe(initial + 1);
  const resumed = await page.evaluate(() => (window as unknown as { layoutProbe: { inits: { x: number; y: number }[][] } }).layoutProbe.inits.at(-1)!);
  expect(resumed.map((node) => [node.x, node.y])).toEqual(resumed.map((_, i) => [1200 + i, 2300 + i]));
  await page.evaluate(() => (window as unknown as { layoutProbe: { ends: (() => void)[] } }).layoutProbe.ends.at(-1)!());
  const settled = await starts();
  label = "Settled refresh applied";
  events.pending = "event: graph:updated\ndata: {}\n\n";
  await expect(jsonResultRegion(page)).toContainText(label);
  expect(await starts()).toBe(settled);
});


test("rank generation handshake refreshes pinned context after a missed recompute", async ({ page, request }) => {
  const symbol = await fetchFirstSymbol(request);
  let ranks = "0";
  let connections = 0;
  let catalogs = 0;
  let contexts = 0;
  const workspace = deliveryWorkspace("all", "all", "Ranks fixture");
  await page.route("**/api/v1/events", (route) => {
    connections += 1;
    return route.fulfill({ status: 200, contentType: "text/event-stream", body:
      `retry: 200\nevent: graph:generation\ndata: ${JSON.stringify({ graph_generation: "1", pagerank_generation: ranks })}\n\n` });
  });
  await page.route("**/api/v1/workspaces", (route) => {
    catalogs += 1;
    return route.fulfill({ json: { workspaces: [workspace], _meta: workspace._meta } });
  });
  await page.route("**/api/v1/brain/context", async (route) => {
    contexts += 1;
    const response = await route.fetch();
    const body = await response.json();
    body.seeds = body.seeds.map((seed: { uid: string; title: string }) => ({ ...seed,
      title: seed.uid === symbol.uid && ranks === "1" ? "Missed ranks applied" : seed.title }));
    await route.fulfill({ response, json: body });
  });
  await openP1Workspace(page);
  await expect.poll(() => catalogs).toBeGreaterThan(1);
  await expect.poll(() => connections).toBeGreaterThan(3);
  const results = await fillSearch(page, symbol.name);
  await results.getByRole("option").first().getByRole("button", { name: "Explore" }).click();
  await selectRepresentation(page, "JSON");
  await waitForJsonPayload(page, (payload) => payload.active_lens.lens === "context" &&
    payload.graph.nodes.some((node) => node.uid === symbol.uid), "pinned rank context loaded");
  const before = { contexts, catalogs };
  ranks = "1"; // No pagerank:recomputed event follows this missed publication.
  await expect(jsonResultRegion(page)).toContainText("Missed ranks applied");
  expect(contexts).toBe(before.contexts + 1);
  expect(catalogs).toBe(before.catalogs);
  const caughtUp = connections;
  await expect.poll(() => connections).toBeGreaterThan(caughtUp + 2);
  expect(contexts).toBe(before.contexts + 1);
});

test("Symbols scope failures never expose the previous workspace catalog", async ({ page }) => {
  const workspaces = [deliveryWorkspace("all", "all", "Symbol scope A"), deliveryWorkspace("repo:failure", "repo", "Symbol scope B")];
  await page.route("**/api/v1/workspaces", (route) => route.fulfill({ json: { workspaces, _meta: workspaces[0]._meta } }));
  let hold = false;
  let pending = false;
  let release: (() => void) | undefined;
  await page.route("**/api/v1/symbols/top?**", async (route) => {
    const workspace = new URL(route.request().url()).searchParams.get("workspace") ?? "all";
    if (workspace === "repo:failure") {
      await route.fulfill({ status: 503, json: { error: "Scope B failed" } });
      return;
    }
    if (hold) { pending = true; await new Promise<void>((resolve) => { release = resolve; }); }
    await route.fulfill({ json: [{ uid: "sym:scope-a", name: "ScopeAWitness", kind: "Function", file_path: "a.ts", start_line: 1 }] });
  });
  await openP1Workspace(page);
  const explorer = page.getByTestId("explorer-panel");
  await explorer.getByRole("tab", { name: "Symbols", exact: true }).click();
  await expect(explorer.getByText("ScopeAWitness", { exact: true })).toBeVisible();
  await selectWorkspace(page, workspaces[1]);
  await expect(explorer).toContainText("Scope B failed");
  await expect(explorer.getByText("ScopeAWitness", { exact: true })).toHaveCount(0);
  hold = true;
  await selectWorkspace(page, workspaces[0]);
  await expect.poll(() => pending).toBe(true);
  await expect(explorer).not.toContainText("Scope B failed");
  release!();
  await expect(explorer.getByText("ScopeAWitness", { exact: true })).toBeVisible();
});


test("same note UID never retains another workspace body or error while loading", async ({ page }) => {
  const workspaces = [deliveryWorkspace("all", "all", "Note scope A"), deliveryWorkspace("vault:note-b", "vault", "Note scope B")];
  await page.route("**/api/v1/workspaces", (route) => route.fulfill({ json: { workspaces, _meta: workspaces[0]._meta } }));
  const witness = { uid: "note:workspace-witness", vault_uid: "vlt:witness", title: "WorkspaceNoteWitness",
    file_path: "workspace.md", note_kind: "General", word_count: 1, content_hash: "fixture", frontmatter: null,
    created_at: null, modified_at: null, pagerank_score: 0 };
  await page.route("**/api/v1/brain/vaults", (route) => route.fulfill({ json: [{ uid: witness.vault_uid, name: "witness", root_path: "/fixture", instance_id: "fixture", note_count: 1 }] }));
  await page.route("**/api/v1/brain/tags", (route) => route.fulfill({ json: [] }));
  await page.route("**/api/v1/brain/notes?**", (route) => route.fulfill({ json: [witness], headers: { "x-total-count": "1" } }));
  let phase = "A";
  let pending = false;
  let release: (() => void) | undefined;
  await page.route("**/api/v1/brain/note/**", async (route) => {
    const requestPhase = phase;
    if (requestPhase === "A-return") { pending = true; await new Promise<void>((resolve) => { release = resolve; }); }
    if (requestPhase === "B") { await route.fulfill({ status: 503, json: { error: "Note scope B failed" } }); return; }
    await route.fulfill({ json: { note: witness, headings: [], sections: [], body: requestPhase === "A" ? "Scope A body witness" : "Scope A reloaded witness" } });
  });
  await openP1Workspace(page);
  const explorer = page.getByTestId("explorer-panel");
  await explorer.getByRole("tab", { name: "Notes", exact: true }).click();
  await explorer.getByText(witness.title, { exact: true }).click();
  const detail = page.getByTestId("detail-panel");
  await expect(detail).toContainText("Scope A body witness");
  phase = "B";
  await selectWorkspace(page, workspaces[1]);
  await expect(detail).toContainText("Note scope B failed");
  await expect(detail).not.toContainText("Scope A body witness");
  phase = "A-return";
  await selectWorkspace(page, workspaces[0]);
  await expect.poll(() => pending).toBe(true);
  await expect(detail).toContainText("Loading note...");
  await expect(detail).not.toContainText("Note scope B failed");
  await expect(detail).not.toContainText("Scope A body witness");
  release!();
  await expect(detail).toContainText("Scope A reloaded witness");
});


test("committed same-scene overview rebuild preserves hub and sibling edge identities", async ({ page }) => {
  const events = await deliveryEvents(page);
  const workspace = deliveryWorkspace("all", "all", "Stable overview fixture");
  let committed = false;
  await page.route("**/api/v1/workspaces", (route) => route.fulfill({ json: { workspaces: [workspace], _meta: workspace._meta } }));
  const repoUid = "repo:fixture:stable";
  const hub = { uid: repoUid, kind: "repo", label: "Stable repository", location: "fixture", score: 1, reason: "edge identity fixture" };
  const members = Array.from({ length: 14 }, (_, index) => ({
    uid: `sym:${repoUid}:${index}`, kind: "symbol", label: `Stable member ${index}`,
    location: "src/stable.ts", score: 1, reason: "edge identity fixture",
  }));
  await page.route("**/api/v1/overview?**", (route) => route.fulfill({ json: {
    counts: { ...workspace.counts, gap_count: 0 }, landmarks: [hub, ...members], start_here: [hub], gaps: [],
    _meta: { ...workspace._meta, trust: { ...workspace._meta.trust, message: committed ? "Identity rebuild accepted" : "Identity original accepted" } },
  } }));
  await openP1Workspace(page);
  await selectRepresentation(page, "JSON");
  const before = await waitForJsonPayload(page, (payload) => payload._meta.trust.message === "Identity original accepted" && payload.graph.edges.length > 14, "fixture contains real hub and synthetic sibling relationships");
  committed = true;
  events.pending = "event: graph:updated\ndata: {}\n\n";
  const after = await waitForJsonPayload(page, (payload) => payload._meta.trust.message === "Identity rebuild accepted", "a new overview response was actually applied to the existing scene");
  expect(after.graph.edges).toEqual(before.graph.edges);
  expect(after.graph.nodes.map((node) => node.uid).sort()).toEqual(before.graph.nodes.map((node) => node.uid).sort());
  expect(after.active_lens).toEqual(before.active_lens);
  expect(after.representation).toBe("json");
});
