import { test, expect } from "@playwright/test";

test.describe("App Health", () => {
  test("API health endpoint responds", async ({ request }) => {
    const response = await request.get("/api/v1/health");
    expect(response.ok()).toBeTruthy();
    const body = await response.json();
    expect(body).toHaveProperty("status", "ok");
    expect(body.product_version).toMatch(/^\d+\.\d+\.\d+/);
    const alias = await request.get("/health");
    expect(alias.headers()["content-type"]).toContain("application/json");
    expect(await alias.json()).toEqual(body);
    const version = await (await request.get("/api/v1/version")).json();
    expect(version.product_version).toBe(body.product_version);
    expect(typeof version.graph_generation).toBe("number");
  });

  test("SPA loads without errors", async ({ page }) => {
    const errors: string[] = [];
    page.on("pageerror", (err) => errors.push(err.message));

    await page.goto("/");
    await expect(page.locator("body")).toBeVisible();
    await expect(
      page.locator('[data-testid="top-bar"]')
    ).toBeVisible({ timeout: 10_000 });

    expect(errors).toEqual([]);
  });

  test("unknown API route returns 404", async ({ request }) => {
    const response = await request.get("/api/v1/nonexistent");
    expect(response.status()).toBe(404);
  });
});
