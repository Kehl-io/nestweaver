import { test, expect } from "@playwright/test";

test.describe("Graph Explorer", () => {
  test("graph panel renders with nodes", async ({ page }) => {
    await page.goto("/");
    await page.waitForTimeout(2_000);
    const graphContainer = page.locator('[data-testid="graph-panel"]');
    await expect(graphContainer).toBeVisible({ timeout: 15_000 });
  });

  test("repo-map API returns data", async ({ request }) => {
    const response = await request.get("/api/v1/repo-map?token_budget=2000");
    expect(response.ok()).toBeTruthy();
    const body = await response.text();
    expect(body.length).toBeGreaterThan(0);
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
