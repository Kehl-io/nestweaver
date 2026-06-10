import { test, expect } from "@playwright/test";

test.describe("Search Flow", () => {
  test("search via API returns results for known symbol", async ({ request }) => {
    const response = await request.get("/api/v1/search?q=greet");
    expect(response.ok()).toBeTruthy();
    const body = await response.json();
    expect(body.length).toBeGreaterThan(0);
    expect(body.some((r: any) => r.name.toLowerCase().includes("greet"))).toBe(true);
  });

  test("search via UI shows results", async ({ page }) => {
    await page.goto("/");
    const searchInput = page.locator('[data-testid="search-input"]');
    await searchInput.waitFor({ timeout: 10_000 });
    await searchInput.fill("greet");
    await expect(page.locator("text=greet").first()).toBeVisible({ timeout: 10_000 });
  });

  test("search with no results shows empty state", async ({ request }) => {
    const response = await request.get("/api/v1/search?q=zzz_nonexistent_symbol_zzz");
    expect(response.ok()).toBeTruthy();
    const body = await response.json();
    expect(body).toEqual([]);
  });

  test("symbol detail loads when clicking a result", async ({ page }) => {
    await page.goto("/");
    const searchInput = page.locator('[data-testid="search-input"]');
    await searchInput.waitFor({ timeout: 10_000 });
    await searchInput.fill("greet");
    const dropdown = page.locator('[role="listbox"]');
    const firstResult = dropdown.locator('[role="option"]').first();
    await firstResult.waitFor({ timeout: 10_000 });
    await firstResult.click();
    await expect(
      page.locator('[data-testid="detail-panel"]')
    ).toBeVisible({ timeout: 10_000 });
  });
});
