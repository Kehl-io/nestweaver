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
        current.representation === "json",
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
