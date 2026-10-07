import { expect, test, type APIRequestContext, type Page } from "@playwright/test";

// nw-593: at a 768px tablet width Files/Symbols/Notes/Evidence/Details all
// disappeared, the four representation tabs had no aria-label or text, and
// the live region never announced search, path or compare results.

interface SymbolCandidate {
  uid: string;
  name: string;
  kind: string;
}

async function findSymbol(request: APIRequestContext, query: string): Promise<SymbolCandidate> {
  for (let attempt = 0; attempt < 40; attempt += 1) {
    const response = await request.get(`/api/v1/search?q=${encodeURIComponent(query)}&limit=8`);
    if (response.ok()) {
      const symbols = (await response.json()) as SymbolCandidate[];
      const match = symbols.find((s) => s.name === query) ?? symbols[0];
      if (match) return match;
    }
    await new Promise((resolve) => setTimeout(resolve, 250));
  }
  throw new Error(`fixture has no searchable symbol for ${query}`);
}

async function open(page: Page, width: number, path: string) {
  await page.setViewportSize({ width, height: 1024 });
  await page.addInitScript(() => {
    window.localStorage.setItem(
      "nestweaver-ui",
      JSON.stringify({
        state: { layoutMode: "panels", representationMode: "graph", viewMode: "graph" },
        version: 6,
      }),
    );
  });
  await page.goto(path);
  await expect(page.getByTestId("graph-panel")).toBeVisible({ timeout: 15_000 });
}

const liveRegion = (page: Page) => page.getByTestId("live-announcer");

test.describe("Tablet width (nw-593)", () => {
  test("a 768px drawer exposes Details and Evidence for the selection", async ({ page, request }) => {
    const symbol = await findSymbol(request, "greet");
    await open(page, 768, `/?node=${encodeURIComponent(symbol.uid)}&kind=${symbol.kind}`);

    const toggle = page.getByRole("button", { name: "Inspector", exact: true });
    await expect(toggle).toHaveAttribute("aria-expanded", "false");
    await toggle.click();
    await expect(toggle).toHaveAttribute("aria-expanded", "true");

    const drawer = page.getByRole("complementary", { name: "Inspector" });
    await expect(drawer).toBeVisible();
    const tabs = drawer.getByRole("tablist", { name: "Inspector panes" });
    await expect(tabs.getByRole("tab", { name: "Details" })).toHaveAttribute("aria-selected", "true");
    await expect(drawer.getByTestId("detail-panel").getByText(symbol.name).first()).toBeVisible();

    await tabs.getByRole("tab", { name: "Evidence" }).click();
    await expect(
      drawer.getByRole("complementary", { name: "Source and note evidence" }),
    ).toBeVisible();

    await page.keyboard.press("Escape");
    await expect(drawer).toBeHidden();
    await expect(toggle).toBeFocused();
  });

  test("reopening the drawer focuses the pane it was left on", async ({ page, request }) => {
    const symbol = await findSymbol(request, "greet");
    await open(page, 768, `/?node=${encodeURIComponent(symbol.uid)}&kind=${symbol.kind}`);
    const toggle = page.getByRole("button", { name: "Inspector", exact: true });
    await toggle.click();
    const tabs = page.getByRole("tablist", { name: "Inspector panes" });
    await tabs.getByRole("tab", { name: "Evidence" }).click();
    await page.getByRole("button", { name: "Close inspector" }).click();
    await expect(toggle).toBeFocused();
    await toggle.click();
    await expect(tabs.getByRole("tab", { name: "Evidence" })).toBeFocused();
    await expect(tabs.getByRole("tab", { name: "Evidence" })).toHaveAttribute("aria-selected", "true");
  });

  test("widening past the breakpoint closes the drawer so narrowing again does not steal focus", async ({
    page,
    request,
  }) => {
    const symbol = await findSymbol(request, "greet");
    await open(page, 768, `/?node=${encodeURIComponent(symbol.uid)}&kind=${symbol.kind}`);
    await page.getByRole("button", { name: "Inspector", exact: true }).click();
    await expect(page.getByRole("complementary", { name: "Inspector" })).toBeVisible();

    await page.setViewportSize({ width: 1440, height: 1024 });
    await expect(page.getByRole("button", { name: "Inspector", exact: true })).toBeHidden();
    await expect(page.getByRole("complementary", { name: "Inspector" })).toHaveCount(0);
    await expect(page.getByTestId("detail-panel")).toBeVisible();
    await page.getByTestId("search-input").focus();
    await page.setViewportSize({ width: 768, height: 1024 });
    const toggle = page.getByRole("button", { name: "Inspector", exact: true });
    await expect(toggle).toHaveAttribute("aria-expanded", "false");
    await expect(page.getByRole("complementary", { name: "Inspector" })).toHaveCount(0);
    await expect(page.getByTestId("search-input")).toBeFocused();
  });

  test("every visible control at 768px has an aria-label or text", async ({ page, request }) => {
    const symbol = await findSymbol(request, "greet");
    for (const path of ["/", `/?node=${encodeURIComponent(symbol.uid)}&kind=${symbol.kind}`]) {
      await open(page, 768, path);
      // Wait for the chrome that loads last: the result tabs and, on a
      // node link, the loaded Details for it.
      await expect(page.getByRole("tablist", { name: "Result representation" })).toBeVisible();
      if (path !== "/") {
        await expect(page.getByRole("navigation", { name: "Scene breadcrumbs" })).toContainText(symbol.name);
      }
      const unnamed = await page.evaluate(() =>
        Array.from(document.querySelectorAll("button, [role=button], [role=tab], a[href]"))
          .filter((el) => {
            const rect = el.getBoundingClientRect();
            if (rect.width === 0 || rect.height === 0) return false;
            return (
              !el.getAttribute("aria-label") &&
              !el.getAttribute("aria-labelledby") &&
              !(el as HTMLElement).innerText.trim()
            );
          })
          .map((el) => el.outerHTML.slice(0, 120)),
      );
      expect(unnamed, `unnamed controls on ${path}`).toEqual([]);
    }
  });

  test("search, path and compare results are announced", async ({ page, request }) => {
    const from = await findSymbol(request, "releaseA");
    await open(page, 1440, `/?node=${encodeURIComponent(from.uid)}&kind=${from.kind}`);
    await expect(page.getByTestId("detail-panel").getByText("releaseA").first()).toBeVisible();

    await page.getByTestId("search-input").fill("releaseB");
    await expect(liveRegion(page)).toContainText(/\d+ results? for "releaseB"/);
    await page.keyboard.press("Escape");

    const actions = page.getByTestId("detail-panel").getByRole("group", { name: "Node actions" }).first();
    await actions.getByRole("button", { name: /^Path$/ }).click();
    const dialog = page.getByRole("dialog", { name: "Find path" });
    await dialog.getByRole("textbox", { name: "Path target" }).fill("releaseC");
    await dialog.getByRole("button", { name: "Find" }).click();
    await expect(liveRegion(page)).toContainText(/Found 1 path/);

    await actions.getByRole("button", { name: /^Compare$/ }).click();
    const seeds = page.getByPlaceholder("Comma-separated seeds...");
    await expect(seeds).toBeEnabled({ timeout: 15_000 });
    await seeds.fill("releaseC");
    await seeds.press("Enter");
    await expect(liveRegion(page)).toContainText(/Compare ready: \d+ shared/, { timeout: 15_000 });
  });

  test("an identical result is announced again", async ({ page, request }) => {
    const from = await findSymbol(request, "releaseA");
    await open(page, 1440, `/?node=${encodeURIComponent(from.uid)}&kind=${from.kind}`);
    await expect(page.getByTestId("detail-panel").getByText("releaseA").first()).toBeVisible();

    const actions = page.getByTestId("detail-panel").getByRole("group", { name: "Node actions" }).first();
    const message = liveRegion(page).locator("[data-message-id]");
    const findPath = async () => {
      await actions.getByRole("button", { name: /^Path$/ }).click();
      const dialog = page.getByRole("dialog", { name: "Find path" });
      await dialog.getByRole("textbox", { name: "Path target" }).fill("releaseC");
      const done = page.waitForResponse((r) => new URL(r.url()).pathname.startsWith("/api/v1/paths/"));
      await dialog.getByRole("button", { name: "Find" }).click();
      await done;
    };

    await findPath();
    await expect(message).toHaveText("Found 1 path.");
    const firstId = await message.getAttribute("data-message-id");
    const firstNode = await message.elementHandle();

    // The second, identical query: wait for the app's own signal (a new
    // message id), then check the text node was replaced, not reused.
    await findPath();
    await expect(message).not.toHaveAttribute("data-message-id", firstId ?? "");
    await expect(message).toHaveText("Found 1 path.");
    expect(await firstNode!.evaluate((node) => node.isConnected)).toBe(false);
    await expect(liveRegion(page).locator("[data-message-id]")).toHaveCount(1);
  });

  test("counterweight: at 1440px the panes stay inline with no drawer", async ({ page, request }) => {
    const symbol = await findSymbol(request, "greet");
    await open(page, 1440, `/?node=${encodeURIComponent(symbol.uid)}&kind=${symbol.kind}`);
    await expect(page.getByTestId("detail-panel")).toBeVisible();
    await expect(page.getByRole("button", { name: "Inspector", exact: true })).toHaveCount(0);
    await expect(page.getByRole("complementary", { name: "Inspector" })).toHaveCount(0);
  });
});

// nw-755: containment must survive long identities, not only the short fixture names.
test("long search identities and paths keep tablet actions operable without overflow", async ({ page }) => {
  // Keep both viewport transitions and every containment assertion on slower CI browsers.
  test.setTimeout(60_000);
  const name = "VeryLongWorkspaceSymbolIdentity".repeat(5);
  const path = `src/${"long-folder-name/".repeat(9)}${name}.ts`;
  await page.route("**/api/v1/workspaces", async (route) => {
    const response = await route.fetch();
    const catalog = await response.json();
    catalog.workspaces = catalog.workspaces.map((w: { id: string; label: string }) =>
      w.id === "all" ? { ...w, label: "VeryLongWorkspaceIdentity".repeat(6) } : w);
    await route.fulfill({ json: catalog });
  });
  await page.route("**/api/v1/search?**", (route) => route.fulfill({ json: [
    { uid: "sym:tablet:long", name, kind: "Function", file_path: path, start_line: 42 },
  ] }));
  await page.route("**/api/v1/brain/search?**", (route) => route.fulfill({ json: [] }));
  await page.route("**/api/v1/symbol/**", (route) => route.fulfill({ json: {
    symbol: { uid: "sym:tablet:long", name, kind: "Function", repo_uid: "repo:tablet", file_path: path,
      start_line: 42, signature: null, summary: null, pagerank_score: 0 }, callers: [], callees: [],
  } }));
  await open(page, 768, "/");
  await page.getByTestId("search-input").fill("long");
  const option = page.getByRole("listbox", { name: "Search results" }).getByRole("option").first();
  await expect(option).toContainText(name);
  await expect(option).toContainText(path);
  for (const title of [name, path]) {
    const span = option.locator(`span[title=${JSON.stringify(title)}]`);
    await expect(span).toHaveAttribute("title", title);
    await expect(span).toBeVisible();
    const box = await span.boundingBox();
    expect(box, `${title} has a rendered box`).not.toBeNull();
    expect(box!.width, `${title} must have visible width`).toBeGreaterThan(0);
    expect(box!.height, `${title} must have visible height`).toBeGreaterThan(0);
    expect(box!.x).toBeGreaterThanOrEqual(0);
    expect(box!.x + box!.width).toBeLessThanOrEqual(768);
  }
  for (const action of ["Detail", "Explore", "Add"]) {
    const button = option.getByRole("button", { name: action, exact: true });
    await expect(button).toBeVisible();
    const box = await button.boundingBox();
    expect(box).not.toBeNull();
    expect(box!.x).toBeGreaterThanOrEqual(0);
    expect(box!.x + box!.width).toBeLessThanOrEqual(768);
  }
  expect(await option.evaluate((el) => el.scrollWidth <= el.clientWidth)).toBe(true);
  await option.getByRole("button", { name: "Detail", exact: true }).click();
  await page.getByRole("button", { name: "Inspector", exact: true }).click();
  await expect(page.getByTestId("detail-panel")).toContainText(name);
  expect(await page.getByTestId("detail-panel").evaluate((el) => el.scrollWidth <= el.clientWidth)).toBe(true);
  await page.setViewportSize({ width: 1440, height: 1024 });
  await expect(page.getByRole("button", { name: "Inspector", exact: true })).toBeHidden();
  await expect(page.getByRole("complementary", { name: "Inspector" })).toHaveCount(0);
  await expect(page.getByTestId("detail-panel")).toBeVisible();
  await page.setViewportSize({ width: 768, height: 1024 });
  await expect(page.getByRole("button", { name: "Inspector", exact: true })).toHaveAttribute("aria-expanded", "false");
  expect(await page.evaluate(() => document.documentElement.scrollWidth <= window.innerWidth)).toBe(true);
});
