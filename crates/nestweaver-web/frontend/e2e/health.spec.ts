import { test, expect } from "@playwright/test";

test.describe("App Health", () => {
  test("API health endpoint responds", async ({ request }) => {
    const response = await request.get("/api/v1/health");
    expect(response.ok()).toBeTruthy();
    const body = await response.json();
    expect(body).toHaveProperty("status", "ok");
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

  test("unknown API route does not return 500", async ({ request }) => {
    const response = await request.get("/api/v1/nonexistent");
    expect(response.status()).not.toBeGreaterThanOrEqual(500);
  });
});
