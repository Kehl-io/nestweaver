import {
  test,
  expect,
  type APIRequestContext,
  type APIResponse,
  type Page,
} from "@playwright/test";
import { readFile } from "node:fs/promises";
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
  return overview.start_here.slice(0, 2);
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

async function openOverview(
  page: Page,
  options: { panels?: boolean } = {},
) {
  await page.goto("/");

  const graphContainer = page.locator('[data-testid="graph-panel"]');
  await expect(graphContainer).toBeVisible({ timeout: 15_000 });

  const dock = page.getByTestId("control-dock");
  await expect(dock).toBeVisible();

  if (!options.panels) {
    await expect(
      page.getByRole("region", { name: "Start Here" }),
    ).toHaveCount(0);
    return;
  }

  await dock.getByRole("button", { name: "Settings" }).click();
  await dock.getByRole("button", { name: "Focus Map" }).click();

  const modeIndicator = page.getByRole("button", {
    name: /Overview/,
  });
  await expect(modeIndicator).toBeVisible();

  // Close the settings flyout by clicking outside it
  await page.locator('[data-testid="graph-panel"]').click({ position: { x: 10, y: 10 } });
}

test.describe("Graph Explorer", () => {
  test("first open renders Focus Map by default", async ({ page, request }) => {
    await fetchOverview(request);
    await openOverview(page);
    await expect(page.getByTestId("control-dock")).toBeVisible();
    await expect(
      page.getByRole("region", { name: "Start Here" }),
    ).toHaveCount(0);
  });

  test("panel overview shows Start Here without an idle context card", async ({
    page,
    request,
  }) => {
    const overview = await fetchOverview(request);
    const visibleItems = displayedStartHereItems(overview);

    await openOverview(page, { panels: true });

    const startHere = page.getByRole("region", { name: "Start Here" });
    await expect(startHere).toBeVisible({ timeout: 15_000 });
    await expect(
      startHere.getByText(`${overview.start_here.length} entry points`),
    ).toBeVisible();

    for (const item of visibleItems) {
      await expect(
        startHere.getByText(item.label, { exact: true }),
      ).toBeVisible();
    }
    if (visibleItems.length === 0) {
      await expect(startHere.getByText("No entry points found.")).toBeVisible();
    }

    await expect(
      page.getByRole("complementary", { name: "Overview context" }),
    ).toHaveCount(0);
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

    await openOverview(page, { panels: true });

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

    await openOverview(page, { panels: true });

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
      page.getByRole("button", { name: /Context/ }),
    ).toBeVisible();
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

    await openOverview(page, { panels: true });

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

    await openOverview(page, { panels: true });
    await page.getByTestId("search-input").fill(firstSymbol.name);

    const option = page
      .getByRole("option", {
        name: new RegExp(escapeRegExp(firstSymbol.name)),
      })
      .first();
    await expect(option).toBeVisible();
    await option.getByRole("button", { name: "Add" }).click();

    await expect(
      page.getByRole("button", { name: /Context/ }),
    ).toBeVisible();
  });

  test("grouped controls switch to list and matrix views", async ({ page }) => {
    await openOverview(page, { panels: true });

    const dock = page.getByTestId("control-dock");

    await dock.getByRole("button", { name: "Settings" }).click();
    await dock.getByRole("button", { name: /List/ }).click();
    await expect(
      page.getByRole("region", { name: "Ranked node table" }),
    ).toBeVisible();

    await dock.getByRole("button", { name: "Settings" }).click();
    await dock.getByRole("button", { name: /Matrix/ }).click();
    await expect(
      page.getByRole("region", { name: "Graph matrix view" }),
    ).toBeVisible();

    await dock.getByRole("button", { name: "Settings" }).click();
    await expect(page.getByLabel("Scope")).toBeVisible();
  });

  test("export menu downloads current graph as PNG, SVG, and HTML", async ({
    page,
  }, testInfo) => {
    await openOverview(page);
    await expect(page.locator("canvas").first()).toBeVisible();
    await expect(page.getByText("js", { exact: true })).toBeVisible({
      timeout: 15_000,
    });

    async function downloadExport(label: RegExp, extension: string) {
      const dock = page.getByTestId("control-dock");
      // Open the settings flyout if not already open
      const flyout = dock.locator(".max-h-\\[70vh\\]");
      if (!(await flyout.isVisible().catch(() => false))) {
        await dock.getByRole("button", { name: "Settings" }).click();
      }
      // Export buttons are directly inside the flyout (no separate Export button)
      const downloadPromise = page.waitForEvent("download");
      await dock.getByRole("button", { name: label }).click();
      const download = await downloadPromise;
      const outputPath = testInfo.outputPath(`nestweaver-graph.${extension}`);
      await download.saveAs(outputPath);
      return readFile(outputPath);
    }

    const png = await downloadExport(/PNG/, "png");
    expect(png.length).toBeGreaterThan(1000);
    expect(Array.from(png.subarray(0, 4))).toEqual([0x89, 0x50, 0x4e, 0x47]);

    const svg = (await downloadExport(/SVG/, "svg")).toString("utf8");
    expect(svg).toContain("<svg");
    expect((svg.match(/<circle\b/g) ?? []).length).toBeGreaterThan(0);
    expect((svg.match(/<line\b/g) ?? []).length).toBeGreaterThan(0);

    const html = (await downloadExport(/HTML/, "html")).toString("utf8");
    expect(html).toContain("<!DOCTYPE html>");
    expect(html).toMatch(/const nodes = \[\{/);
    expect(html).toContain("const edges = [");
  });

  test("ranked table sorts and selects nodes", async ({ page }) => {
    await openOverview(page, { panels: true });

    const dock = page.getByTestId("control-dock");
    await dock.getByRole("button", { name: "Settings" }).click();
    await dock.getByRole("button", { name: /List/ }).click();

    const table = page.getByRole("region", { name: "Ranked node table" });
    await expect(table).toBeVisible();
    await table
      .locator("thead")
      .getByRole("button", { name: "Name", exact: true })
      .click();
    const firstRowButton = table.locator("tbody button").first();
    await expect(firstRowButton).toBeVisible();
    await firstRowButton.click();
  });

  test("matrix view renders bounded nodes and row selection", async ({
    page,
  }) => {
    await openOverview(page, { panels: true });

    const dock = page.getByTestId("control-dock");
    await dock.getByRole("button", { name: "Settings" }).click();
    await dock.getByRole("button", { name: /Matrix/ }).click();

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

    await openOverview(page, { panels: true });

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

    await expect(
      page.getByRole("complementary", { name: "Overview context" }),
    ).toHaveCount(0);
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

    await openOverview(page, { panels: true });

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
