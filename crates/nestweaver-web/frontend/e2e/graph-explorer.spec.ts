import {
  test,
  expect,
  type APIRequestContext,
  type Page,
} from "@playwright/test";
import type { OverviewLandmark, OverviewResponse } from "../src/api/types";

async function fetchOverview(
  request: APIRequestContext,
): Promise<OverviewResponse> {
  let lastStatus = 0;

  for (let attempt = 0; attempt < 40; attempt += 1) {
    const response = await request.get("/api/v1/overview?limit=24");
    lastStatus = response.status();

    if (response.ok()) {
      const overview = (await response.json()) as OverviewResponse;
      expect(overview).toHaveProperty("counts");
      expect(Array.isArray(overview.landmarks)).toBeTruthy();
      expect(Array.isArray(overview.start_here)).toBeTruthy();
      expect(Array.isArray(overview.gaps)).toBeTruthy();
      return overview;
    }

    await new Promise((resolve) => setTimeout(resolve, 250));
  }

  throw new Error(`overview API did not become ready; last status ${lastStatus}`);
}

function displayedStartHereItems(
  overview: OverviewResponse,
): OverviewLandmark[] {
  return overview.start_here.slice(0, 7);
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
    const response = await request.get("/api/v1/repo-map?token_budget=2000");
    expect(response.ok()).toBeTruthy();
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
    const response = await request.post("/api/v1/context", {
      data: { seeds: ["greet"], limit: 50 },
    });
    expect(response.ok()).toBeTruthy();
    const body = await response.json();
    expect(body).toHaveProperty("seeds");
    expect(body).toHaveProperty("connected");
    expect(body.seeds.length + body.connected.length).toBeGreaterThan(0);
  });

  test("impact analysis API works", async ({ request }) => {
    const searchResponse = await request.get("/api/v1/search?q=greet");
    const symbols = await searchResponse.json();
    if (symbols.length === 0) {
      test.skip();
      return;
    }
    const uid = symbols[0].uid;
    const impactResponse = await request.get(`/api/v1/impact/${uid}?depth=2`);
    expect(impactResponse.ok()).toBeTruthy();
  });
});
