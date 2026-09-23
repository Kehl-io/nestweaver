import { test, expect } from "@playwright/test";

function symbolPayload(hit: {
  uid: string;
  name: string;
  kind: string;
  file_path: string;
  start_line: number;
}) {
  return {
    symbol: {
      uid: hit.uid,
      name: hit.name,
      kind: hit.kind,
      repo_uid: "repo:test",
      file_path: hit.file_path,
      start_line: hit.start_line,
      signature: null,
      summary: null,
      pagerank_score: 0,
    },
    callers: [],
    callees: [],
  };
}

test.describe("Search Flow", () => {
  test("search via API returns results for known symbol", async ({ request }) => {
    const response = await request.get("/api/v1/search?q=greet");
    expect(response.ok()).toBeTruthy();
    const body = await response.json();
    expect(body.length).toBeGreaterThan(0);
    expect(body.some((r: { name: string }) => r.name.toLowerCase().includes("greet"))).toBe(true);
  });

  test("search via UI shows results", async ({ page }) => {
    await page.goto("/");
    await expect(page.getByTestId("control-dock")).toBeVisible({
      timeout: 15_000,
    });

    const searchInput = page.getByTestId("search-input");
    await searchInput.fill("greet");
    await expect(
      page.getByRole("listbox").getByRole("option").filter({ hasText: "greet" }).first(),
    ).toBeVisible({ timeout: 10_000 });
  });

  test("search with no results shows empty state", async ({ request }) => {
    const response = await request.get("/api/v1/search?q=zzz_nonexistent_symbol_zzz");
    expect(response.ok()).toBeTruthy();
    const body = await response.json();
    expect(body).toEqual([]);
  });

  test("symbol detail loads when clicking a result", async ({ page }) => {
    await page.goto("/");
    const dock = page.getByTestId("control-dock");
    await expect(dock).toBeVisible({ timeout: 15_000 });

    // Panels mode is the default; the detail panel is already visible

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

  test("Code only omits brain search and Notes only omits code search", async ({ page }) => {
    const codeUrls: string[] = [];
    const brainUrls: string[] = [];
    await page.route("**/api/v1/search?**", async (route) => {
      codeUrls.push(route.request().url());
      await route.continue();
    });
    await page.route("**/api/v1/brain/search?**", async (route) => {
      brainUrls.push(route.request().url());
      await route.continue();
    });

    await page.goto("/");
    await expect(page.getByTestId("control-dock")).toBeVisible({ timeout: 15_000 });

    await page.getByLabel("Search filter").click();
    await page.getByRole("option", { name: "Code only" }).click();
    await page.getByTestId("search-input").fill("greet");
    await expect(
      page.getByRole("listbox").getByRole("option").first(),
    ).toBeVisible({ timeout: 10_000 });
    await expect.poll(() => codeUrls.length).toBeGreaterThan(0);
    expect(brainUrls).toEqual([]);

    codeUrls.length = 0;
    brainUrls.length = 0;
    await page.getByLabel("Search filter").click();
    await page.getByRole("option", { name: "All" }).click();
    await expect.poll(() => codeUrls.length).toBeGreaterThan(0);
    await expect.poll(() => brainUrls.length).toBeGreaterThan(0);

    codeUrls.length = 0;
    brainUrls.length = 0;
    await page.getByLabel("Search filter").click();
    await page.getByRole("option", { name: "Notes only" }).click();
    await expect.poll(() => brainUrls.length).toBeGreaterThan(0);
    expect(codeUrls).toEqual([]);
  });

  test("rapid Detail follows the newly selected symbol", async ({ page }) => {
    const first = {
      uid: "sym:a.ts:First",
      name: "First",
      kind: "Function",
      file_path: "a.ts",
      start_line: 14,
    };
    const second = {
      uid: "sym:b.ts:Second",
      name: "Second",
      kind: "Function",
      file_path: "b.ts",
      start_line: 22,
    };

    await page.route("**/api/v1/search?**", async (route) => {
      const q = new URL(route.request().url()).searchParams.get("q") ?? "";
      const hit = q.toLowerCase().includes("second") ? second : first;
      await route.fulfill({ json: [hit] });
    });
    await page.route("**/api/v1/brain/search?**", async (route) => {
      await route.fulfill({ json: [] });
    });
    await page.route("**/api/v1/symbol/**", async (route) => {
      const uid = decodeURIComponent(route.request().url().split("/symbol/")[1] ?? "");
      const hit = uid.includes("Second") ? second : first;
      if (hit === first) {
        await new Promise((resolve) => setTimeout(resolve, 750));
      }
      await route.fulfill({ json: symbolPayload(hit) });
    });

    await page.goto("/");
    await expect(page.getByTestId("control-dock")).toBeVisible({ timeout: 15_000 });

    const searchInput = page.getByTestId("search-input");
    await searchInput.fill("First");
    const firstOption = page.getByRole("listbox").getByRole("option").filter({ hasText: "First" });
    await expect(firstOption).toBeVisible({ timeout: 10_000 });
    await firstOption.getByRole("button", { name: "Detail" }).click();

    await searchInput.fill("Second");
    const secondOption = page.getByRole("listbox").getByRole("option").filter({ hasText: "Second" });
    await expect(secondOption).toBeVisible({ timeout: 10_000 });
    await secondOption.getByRole("button", { name: "Detail" }).click();

    await expect
      .poll(() => new URL(page.url()).searchParams.get("node"))
      .toBe(second.uid);
    await expect(page.getByRole("complementary", { name: "Source and note evidence" })).toContainText(
      "Second",
    );
  });

  test("clicking Detail dismisses the search dropdown (nw-532)", async ({ page }) => {
    await page.goto("/");
    await expect(page.getByTestId("control-dock")).toBeVisible({ timeout: 15_000 });

    const searchInput = page.getByTestId("search-input");
    await searchInput.fill("greet");
    const dropdown = page.getByRole("listbox");
    const firstOption = dropdown.getByRole("option").first();
    await expect(firstOption).toBeVisible({ timeout: 10_000 });
    await firstOption.getByRole("button", { name: "Detail" }).click();

    // Detail should dismiss the dropdown immediately, not leave it open
    // intercepting clicks on the Graph/Table/Matrix/JSON views underneath.
    await expect(dropdown).toBeHidden();
  });

  test("Escape closes the search overlay without clearing the selection; a second Escape then clears it (nw-532)", async ({
    page,
  }) => {
    await page.goto("/");
    await expect(page.getByTestId("control-dock")).toBeVisible({ timeout: 15_000 });

    const searchInput = page.getByTestId("search-input");
    const dropdown = page.getByRole("listbox");
    const evidencePanel = page.getByRole("complementary", { name: "Source and note evidence" });

    // Select a node via search (this closes the dropdown, same as clicking
    // "Explore" or a result row does today).
    await searchInput.fill("greet");
    const firstOption = dropdown.getByRole("option").first();
    await expect(firstOption).toBeVisible({ timeout: 10_000 });
    await firstOption.click();
    await expect
      .poll(() => new URL(page.url()).searchParams.get("node"))
      .not.toBeNull();
    const selectedUid = new URL(page.url()).searchParams.get("node");
    await expect(evidencePanel).not.toContainText("No selection");

    // Reopen the search dropdown (e.g. looking something else up) without
    // picking a result from it — a node stays selected underneath an open
    // search overlay, the exact state the bug report reproduced from.
    await searchInput.fill("greet");
    await expect(dropdown).toBeVisible({ timeout: 10_000 });

    // Escape must close the overlay, not the selection underneath it.
    await page.keyboard.press("Escape");
    await expect(dropdown).toBeHidden();
    await expect(evidencePanel).not.toContainText("No selection");
    expect(new URL(page.url()).searchParams.get("node")).toBe(selectedUid);

    // Counterweight: Escape still clears the selection once nothing else
    // (search, perspectives, etc.) is open to consume it.
    await page.keyboard.press("Escape");
    await expect(evidencePanel).toContainText("No selection");
  });

  test("Escape with nothing selected still closes the search dropdown (nw-532 counterweight)", async ({
    page,
  }) => {
    await page.goto("/");
    await expect(page.getByTestId("control-dock")).toBeVisible({ timeout: 15_000 });

    const searchInput = page.getByTestId("search-input");
    await searchInput.fill("greet");
    const dropdown = page.getByRole("listbox");
    await expect(dropdown).toBeVisible({ timeout: 10_000 });

    await page.keyboard.press("Escape");
    await expect(dropdown).toBeHidden();
  });
});
