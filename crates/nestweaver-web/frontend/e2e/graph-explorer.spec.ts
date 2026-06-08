import {
  test,
  expect,
  type APIRequestContext,
  type APIResponse,
  type Page,
} from "@playwright/test";
import type { OverviewLandmark, OverviewResponse } from "../src/api/types";

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

async function postOk(
  request: APIRequestContext,
  path: string,
  data: unknown,
): Promise<APIResponse> {
  return waitForOk(() => request.post(path, { data }), path);
}

async function fetchOverview(
  request: APIRequestContext,
): Promise<OverviewResponse> {
  const response = await getOk(request, "/api/v1/overview?limit=24");
  const overview = (await response.json()) as OverviewResponse;
  expect(overview).toHaveProperty("counts");
  expect(Array.isArray(overview.landmarks)).toBeTruthy();
  expect(Array.isArray(overview.start_here)).toBeTruthy();
  expect(Array.isArray(overview.gaps)).toBeTruthy();
  return overview;
}

function displayedStartHereItems(
  overview: OverviewResponse,
): OverviewLandmark[] {
  return overview.start_here.slice(0, 7);
}

function emptyOverview(): OverviewResponse {
  return {
    counts: {
      repo_count: 0,
      service_count: 0,
      vault_count: 0,
      note_count: 0,
      symbol_count: 0,
      gap_count: 0,
    },
    landmarks: [],
    start_here: [],
    gaps: [],
  };
}

function escapeRegExp(value: string): string {
  return value.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
}

async function openOverview(page: Page) {
  await page.goto("/");

  const graphContainer = page.locator('[data-testid="graph-panel"]');
  await expect(graphContainer).toBeVisible({ timeout: 15_000 });

  const overviewMode = page.getByRole("button", {
    name: "Overview",
    exact: true,
  });
  await expect(overviewMode).toBeVisible();
  await expect(overviewMode).toHaveClass(/border-blue-500/);
}

test.describe("Graph Explorer", () => {
  test("graph panel renders with nodes", async ({ page, request }) => {
    await fetchOverview(request);
    await openOverview(page);
  });

  test("first open shows overview mode with Start Here and context", async ({
    page,
    request,
  }) => {
    const overview = await fetchOverview(request);
    const visibleItems = displayedStartHereItems(overview);

    await openOverview(page);

    const startHere = page.getByRole("region", { name: "Start Here" });
    await expect(startHere).toBeVisible({ timeout: 15_000 });
    await expect(
      startHere.getByText(`${overview.start_here.length} entry points`),
    ).toBeVisible();

    for (const item of visibleItems.slice(0, 3)) {
      await expect(
        startHere.getByText(item.label, { exact: true }),
      ).toBeVisible();
    }
    if (visibleItems.length === 0) {
      await expect(startHere.getByText("No entry points found.")).toBeVisible();
    }

    const contextSurface = page.getByRole("complementary", {
      name: "Overview context",
    });
    await expect(contextSurface).toBeVisible();
    await expect(
      contextSurface.getByRole("heading", { name: "Overview Map" }),
    ).toBeVisible();
    await expect(
      contextSurface.getByText(`${overview.landmarks.length} landmarks`),
    ).toBeVisible();
    await expect(contextSurface.getByText("Repos")).toBeVisible();
    await expect(contextSurface.getByText("Symbols")).toBeVisible();
  });

  test("clicking a Start Here item updates overview context", async ({
    page,
    request,
  }) => {
    const overview = await fetchOverview(request);
    const firstItem = displayedStartHereItems(overview)[0];

    if (!firstItem) {
      test.skip(true, "overview fixture has no visible Start Here items");
      return;
    }

    await openOverview(page);

    const startHere = page.getByRole("region", { name: "Start Here" });
    await expect(
      startHere.getByText(firstItem.label, { exact: true }),
    ).toBeVisible();
    await startHere
      .getByRole("button", {
        name: new RegExp(escapeRegExp(firstItem.label)),
      })
      .first()
      .click();

    const contextSurface = page.getByRole("complementary", {
      name: "Overview context",
    });
    await expect(
      contextSurface.getByRole("heading", { name: firstItem.label }),
    ).toBeVisible();
    await expect(
      contextSurface.getByText(firstItem.kind, { exact: true }),
    ).toBeVisible();
    await expect(
      contextSurface.getByText(firstItem.reason, { exact: true }),
    ).toBeVisible();
    await expect(
      contextSurface.getByText("Overview", { exact: true }),
    ).toBeVisible();
  });

  test("selecting a search result opens an explorable context scene", async ({
    page,
    request,
  }) => {
    const searchResponse = await getOk(request, "/api/v1/search?q=greet");
    const symbols = await searchResponse.json();
    const firstSymbol = symbols[0];

    if (!firstSymbol) {
      test.skip(true, "fixture has no searchable symbol");
      return;
    }

    await openOverview(page);

    await page.getByTestId("search-input").fill(firstSymbol.name);
    const results = page.getByRole("listbox", { name: "Search results" });
    await expect(results).toBeVisible();
    await results
      .getByRole("option", {
        name: new RegExp(escapeRegExp(firstSymbol.name)),
      })
      .first()
      .click();

    await expect(
      page.getByRole("button", { name: "Context", exact: true }),
    ).toHaveClass(/border-blue-500/);
    await postOk(request, "/api/v1/context", {
      seeds: [firstSymbol.uid],
      limit: 50,
    });
  });

  test("selected overview node exposes contextual actions", async ({
    page,
    request,
  }) => {
    const overview = await fetchOverview(request);
    const firstItem = displayedStartHereItems(overview)[0];

    if (!firstItem) {
      test.skip(true, "overview fixture has no visible Start Here items");
      return;
    }

    await openOverview(page);

    const startHere = page.getByRole("region", { name: "Start Here" });
    await startHere
      .getByRole("button", {
        name: new RegExp(escapeRegExp(firstItem.label)),
      })
      .first()
      .click();

    const contextSurface = page.getByRole("complementary", {
      name: "Overview context",
    });
    await expect(
      contextSurface.getByRole("button", { name: "Explore" }).first(),
    ).toBeVisible();
    await expect(
      contextSurface.getByRole("button", { name: "Ask" }).first(),
    ).toBeVisible();
  });

  test("search result secondary Add action builds current scene", async ({
    page,
    request,
  }) => {
    const searchResponse = await getOk(request, "/api/v1/search?q=greet");
    const symbols = await searchResponse.json();
    const firstSymbol = symbols[0];

    if (!firstSymbol) {
      test.skip(true, "fixture has no searchable symbol");
      return;
    }

    await openOverview(page);
    await page.getByTestId("search-input").fill(firstSymbol.name);

    const option = page
      .getByRole("option", {
        name: new RegExp(escapeRegExp(firstSymbol.name)),
      })
      .first();
    await expect(option).toBeVisible();
    await option.getByRole("button", { name: "Add" }).click();

    await expect(
      page.getByRole("button", { name: "Context", exact: true }),
    ).toHaveClass(/border-blue-500/);
  });

  test("grouped controls switch to list and matrix views", async ({ page }) => {
    await openOverview(page);

    await page.getByRole("button", { name: "View" }).click();
    await page.getByRole("button", { name: "List" }).click();
    await expect(
      page.getByRole("region", { name: "Ranked node table" }),
    ).toBeVisible();

    await page.getByRole("button", { name: "View" }).click();
    await page.getByRole("button", { name: "Matrix" }).click();
    await expect(
      page.getByRole("region", { name: "Graph matrix view" }),
    ).toBeVisible();

    await page.getByRole("button", { name: "Filter" }).click();
    await expect(page.getByLabel("Scope")).toBeVisible();
  });

  test("ranked table sorts and selects nodes", async ({ page }) => {
    await openOverview(page);

    await page.getByRole("button", { name: "View" }).click();
    await page.getByRole("button", { name: "List" }).click();

    const table = page.getByRole("region", { name: "Ranked node table" });
    await expect(table).toBeVisible();
    await table.getByRole("button", { name: "Name" }).click();
    const firstRowButton = table.locator("tbody button").first();
    await expect(firstRowButton).toBeVisible();
    await firstRowButton.click();
  });

  test("matrix view renders bounded nodes and row selection", async ({
    page,
  }) => {
    await openOverview(page);

    await page.getByRole("button", { name: "View" }).click();
    await page.getByRole("button", { name: "Matrix" }).click();

    const matrix = page.getByRole("region", { name: "Graph matrix view" });
    await expect(matrix).toBeVisible();
    await expect(matrix.getByText(/Showing top \d+ of \d+/)).toBeVisible();
    const firstRowButton = matrix.locator("tbody th button").first();
    await expect(firstRowButton).toBeVisible();
    await firstRowButton.click();
  });

  test("empty overview shows setup steps with retry", async ({ page }) => {
    await page.route("**/api/v1/overview**", async (route) => {
      await route.fulfill({ json: emptyOverview() });
    });

    await openOverview(page);

    const startHere = page.getByRole("region", { name: "Start Here" });
    await expect(startHere.getByText("No indexed content")).toBeVisible();
    await expect(
      startHere.getByText("Index a project or add a vault"),
    ).toBeVisible();
    await expect(
      startHere.getByText("nestweaver index --repo ."),
    ).toBeVisible();
    await expect(
      startHere.getByRole("button", { name: "Retry overview" }),
    ).toBeVisible();

    const contextSurface = page.getByRole("complementary", {
      name: "Overview context",
    });
    await expect(
      contextSurface.getByText("No indexed content is available yet."),
    ).toBeVisible();
    await expect(
      contextSurface.getByRole("button", { name: "Retry overview" }),
    ).toBeVisible();
  });

  test("repo and service Start Here items do not enable Impact", async ({
    page,
    request,
  }) => {
    const overview = await fetchOverview(request);
    const unsupportedItem = displayedStartHereItems(overview).find(
      (item) => item.kind === "repo" || item.kind === "service",
    );

    if (!unsupportedItem) {
      test.skip(
        true,
        "fixture does not expose a repo or service in visible Start Here items",
      );
      return;
    }

    await openOverview(page);

    const startHere = page.getByRole("region", { name: "Start Here" });
    await startHere
      .getByRole("button", {
        name: new RegExp(escapeRegExp(unsupportedItem.label)),
      })
      .first()
      .click();

    const contextSurface = page.getByRole("complementary", {
      name: "Overview context",
    });
    await expect(
      contextSurface.getByRole("heading", { name: unsupportedItem.label }),
    ).toBeVisible();
    await expect(
      contextSurface.getByRole("button", { name: "Impact" }),
    ).toBeDisabled();
  });

  test("repo-map API returns data", async ({ request }) => {
    const response = await getOk(request, "/api/v1/repo-map?token_budget=2000");
    const body = await response.text();
    expect(body.length).toBeGreaterThan(0);
  });

  test("overview API returns Start Here data", async ({ request }) => {
    const overview = await fetchOverview(request);

    if (overview.landmarks.length === 0) {
      test.skip(true, "overview fixture is empty");
      return;
    }

    expect(overview.start_here.length).toBeGreaterThan(0);
    expect(overview.start_here.length).toBeLessThanOrEqual(
      overview.landmarks.length,
    );
    expect(
      overview.start_here.some((item) => item.kind === "symbol"),
    ).toBeTruthy();
  });

  test("context API returns ranked symbols", async ({ request }) => {
    const response = await postOk(request, "/api/v1/context", {
      seeds: ["greet"],
      limit: 50,
    });
    const body = await response.json();
    expect(body).toHaveProperty("seeds");
    expect(body).toHaveProperty("connected");
    expect(body.seeds.length + body.connected.length).toBeGreaterThan(0);
  });

  test("impact analysis API works", async ({ request }) => {
    const searchResponse = await getOk(request, "/api/v1/search?q=greet");
    const symbols = await searchResponse.json();
    if (symbols.length === 0) {
      test.skip();
      return;
    }
    const uid = symbols[0].uid;
    await getOk(request, `/api/v1/impact/${uid}?depth=2`);
  });
});
