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

  test("Escape closes the dropdown left open by Add without clearing the selection it made (nw-532)", async ({
    page,
  }) => {
    await page.goto("/");
    await expect(page.getByTestId("control-dock")).toBeVisible({ timeout: 15_000 });

    const searchInput = page.getByTestId("search-input");
    const dropdown = page.getByRole("listbox");
    const evidencePanel = page.getByRole("complementary", { name: "Source and note evidence" });

    // "Add" (unlike "Detail") selects the result and deliberately leaves the
    // dropdown open — the exact repro from the bug report: focus lands on a
    // non-form "Add" button while the dropdown, and the selection it just
    // made, both stay live.
    await searchInput.fill("greet");
    const firstOption = dropdown.getByRole("option").first();
    await expect(firstOption).toBeVisible({ timeout: 10_000 });
    await firstOption.getByRole("button", { name: "Add" }).click();

    await expect
      .poll(() => new URL(page.url()).searchParams.get("node"))
      .not.toBeNull();
    const selectedUid = new URL(page.url()).searchParams.get("node");
    await expect(evidencePanel).not.toContainText("No selection");
    await expect(dropdown).toBeVisible();

    // Escape must close the dropdown left open by Add without dropping the
    // selection Add just made — this is the path a render-cycle-later state
    // gate cannot reliably cover, because both the overlay-close handler and
    // the global deselect handler fire on this same keypress.
    await page.keyboard.press("Escape");
    await expect(dropdown).toBeHidden();
    await expect(evidencePanel).not.toContainText("No selection");
    expect(new URL(page.url()).searchParams.get("node")).toBe(selectedUid);
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

  test("Escape closes the Perspectives popover without clearing the selection (nw-532)", async ({
    page,
  }) => {
    await page.goto("/");
    await expect(page.getByTestId("control-dock")).toBeVisible({ timeout: 15_000 });

    const searchInput = page.getByTestId("search-input");
    const dropdown = page.getByRole("listbox");
    const evidencePanel = page.getByRole("complementary", { name: "Source and note evidence" });

    // Select a node first (search closes itself on Explore).
    await searchInput.fill("greet");
    const firstOption = dropdown.getByRole("option").first();
    await expect(firstOption).toBeVisible({ timeout: 10_000 });
    await firstOption.click();
    await expect
      .poll(() => new URL(page.url()).searchParams.get("node"))
      .not.toBeNull();
    const selectedUid = new URL(page.url()).searchParams.get("node");
    await expect(evidencePanel).not.toContainText("No selection");

    // Open the Perspectives popover — focus lands on its toggle button, a
    // non-form element, the same shape of repro the bug report described
    // ("Same when dismissing Perspectives").
    const perspectivesButton = page.getByRole("button", { name: "Perspectives" });
    await perspectivesButton.click();
    const saveCurrentView = page.getByRole("button", { name: "Save current view" });
    await expect(saveCurrentView).toBeVisible();

    // Escape must close the popover, not the selection underneath it.
    await page.keyboard.press("Escape");
    await expect(saveCurrentView).toBeHidden();
    await expect(evidencePanel).not.toContainText("No selection");
    expect(new URL(page.url()).searchParams.get("node")).toBe(selectedUid);

    // Counterweight: with no overlay open, Escape still clears the selection.
    await page.keyboard.press("Escape");
    await expect(evidencePanel).toContainText("No selection");
  });
});

// nw-021: selecting evidence must not replace the current analysis scene.
test("Detail preserves scene identities, lens and representation while Explore and Add navigate", async ({ page }) => {
  await page.setViewportSize({ width: 1440, height: 900 });
  await page.goto("/?representation=json");
  const json = page.getByRole("region", { name: "JSON result", exact: true }).locator("code");
  await expect(json).toBeVisible({ timeout: 15_000 });
  const read = async () => JSON.parse((await json.textContent()) ?? "{}");
  await expect.poll(async () => (await read()).graph.nodes.length).toBeGreaterThan(0);
  const before = await read();
  await page.getByTestId("search-input").fill("greet");
  const results = page.getByRole("listbox", { name: "Search results" });
  const option = results.getByRole("option").first();
  await expect(option).toBeVisible();
  await option.getByRole("button", { name: "Detail" }).click();
  await expect(results).toBeHidden();
  await expect.poll(async () => (await read()).selected_node.uid).not.toBeNull();
  const after = await read();
  expect(after.graph.nodes.map((n: { uid: string }) => n.uid).sort()).toEqual(
    before.graph.nodes.map((n: { uid: string }) => n.uid).sort(),
  );
  expect(after.graph.edges).toEqual(before.graph.edges);
  expect(after.active_lens).toEqual(before.active_lens);
  expect(after.representation).toBe("json");
  await expect(page.getByRole("complementary", { name: "Source and note evidence" })).toContainText("greet");

  await page.getByTestId("search-input").focus();
  await expect(results).toBeVisible();
  await option.getByRole("button", { name: "Explore" }).click();
  await expect(results).toBeHidden();
  await expect.poll(() => new URL(page.url()).searchParams.get("mode")).toBe("context");
  await expect.poll(async () => (await read()).active_lens.lens).toBe("context");
  await page.getByTestId("search-input").fill("releaseA");
  await expect(option).toContainText("releaseA");
  await option.getByRole("button", { name: "Add" }).click();
  await expect(results).toBeVisible();
  await expect.poll(async () => (await read()).active_lens.lens).toBe("search");
  await expect.poll(async () => (await read()).graph.nodes.some((n: { label: string }) => n.label === "releaseA")).toBe(true);
});

test("committed refresh updates held evidence and search without reopening a dismissed dropdown", async ({ page }) => {
  let committed = false;
  const events = { pending: "" };
  await page.route("**/api/v1/events", (route) => {
    const body = `retry: 200\n${events.pending}\n`;
    events.pending = "";
    return route.fulfill({ contentType: "text/event-stream", body });
  });
  const hit = () => ({ uid: "sym:held:identity", name: committed ? "Held committed name" : "Held previous name",
    kind: "Function", file_path: "held.ts", start_line: 12 });
  await page.route("**/api/v1/search?**", (route) => route.fulfill({ json: [hit()] }));
  await page.route("**/api/v1/brain/search?**", (route) => route.fulfill({ json: [] }));
  await page.route("**/api/v1/symbol/**", (route) => route.fulfill({ json: symbolPayload(hit()) }));
  await page.goto("/?representation=json");
  const json = page.getByRole("region", { name: "JSON result", exact: true }).locator("code");
  await expect(json).toBeVisible({ timeout: 15_000 });
  await expect.poll(async () => JSON.parse((await json.textContent()) ?? "{}").graph.nodes.length).toBeGreaterThan(0);
  await page.getByTestId("search-input").fill("Held");
  const dropdown = page.getByRole("listbox", { name: "Search results" });
  await expect(dropdown).toContainText("Held previous name");
  await dropdown.getByRole("button", { name: "Detail", exact: true }).click();
  const evidence = page.getByRole("complementary", { name: "Source and note evidence" });
  await expect(evidence).toContainText("Held previous name");
  committed = true;
  events.pending = "event: graph:updated\ndata: {}\n\nevent: full_refresh\ndata: {}\n\n";
  await expect(evidence).toContainText("Held committed name");
  await expect(dropdown).toBeHidden();
  await expect.poll(() => new URL(page.url()).searchParams.get("node")).toBe("sym:held:identity");
  await expect(page.getByRole("tab", { name: "JSON representation", exact: true })).toHaveAttribute("aria-selected", "true");
  await page.getByTestId("search-input").focus();
  await expect(dropdown).toContainText("Held committed name");
  await expect(dropdown).not.toContainText("Held previous name");
});
