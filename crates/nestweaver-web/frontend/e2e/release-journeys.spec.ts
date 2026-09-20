import { test, expect } from "@playwright/test";

// Baseline capture must run against the embedded assets in the supplied binary.
// Do not mock preview/context responses: the original failure is a boundary bug.
for (const representation of ["JSON", "Table", "Matrix", "Graph"]) {
  test(`release Compare from ${representation}`, async ({ page, request }, testInfo) => {
    const events: unknown[] = [];
    const errors: string[] = [];
    page.on("pageerror", (error) => {
      errors.push(error.stack ?? error.message);
      events.push({ type: "pageerror", stack: error.stack, message: error.message });
    });
    page.on("console", (message) => {
      if (message.type() === "error") {
        if (/TypeError|ReferenceError|RangeError|SyntaxError|Cannot read properties/.test(message.text())) errors.push(message.text());
        events.push({ type: "console", text: message.text(), location: message.location() });
      }
    });
    const responses: Promise<void>[] = [];
    page.on("response", (response) => {
      if (/\/api\/v1\/(brain\/context|symbol\/|source)/.test(response.url())) {
        responses.push(response.text().then((body) => {
          events.push({ type: "response", url: response.url(), status: response.status(), body });
        }).catch((error) => {
          events.push({ type: "response-unavailable", url: response.url(), error: String(error) });
        }));
      }
    });
    try {
      const response = await request.get("/api/v1/search?q=releaseA");
      expect(response.ok()).toBeTruthy();
      const symbols = await response.json();
      expect(symbols.some((symbol: { name: string }) => symbol.name === "releaseA")).toBeTruthy();
      await page.goto("/");
      await page.getByTestId("search-input").fill("releaseA");
      await page.getByRole("listbox", { name: "Search results" })
        .getByRole("option", { name: /releaseA/ }).first().click();
      await page.getByRole("tablist", { name: "Result representation" })
        .getByRole("tab", { name: `${representation} representation`, exact: true }).click();
      await page.getByRole("button", { name: "Compare", exact: true }).first().click();
      const input = page.getByPlaceholder("Comma-separated seeds...");
      await expect(input).toBeVisible();
      await expect(input).toBeEnabled();
      await input.fill("releaseC");
      await input.press("Enter");
      await expect(input).toBeHidden();
      await expect(page.getByRole("region", { name: "Context comparison result" })).toBeVisible();
      await expect(page.getByRole("region", { name: "Context comparison result" })).toContainText("shared");
      await page.getByRole("region", { name: "Context comparison result" }).getByRole("button", { name: "Clear", exact: true }).click();
      await page.getByRole("button", { name: "Compare", exact: true }).first().click();
      await expect(page.getByRole("dialog", { name: "Compare context" })).toBeVisible();
      await page.getByRole("dialog", { name: "Compare context" }).getByRole("button", { name: "Cancel" }).click();
      await expect(page.getByRole("dialog", { name: "Compare context" })).toBeHidden();
      expect(errors, "uncaught browser errors").toEqual([]);
    } finally {
      await Promise.allSettled(responses);
      await testInfo.attach("browser-events.json", {
        body: JSON.stringify(events, null, 2), contentType: "application/json",
      });
      if (!page.isClosed()) {
        await page.content().then((body) => testInfo.attach("page.html", {
          body, contentType: "text/html",
        })).catch(() => undefined);
        await page.screenshot({ fullPage: true }).then((body) => testInfo.attach("journey.png", {
          body, contentType: "image/png",
        })).catch(() => undefined);
      }
    }
  });
}

for (const firstContext of ["loading", "ready"]) {
  test(`release late scene context preserves Compare while its first context is ${firstContext}`, async ({ page }) => {
    const errors: string[] = [];
    page.on("pageerror", (error) => errors.push(error.message));
    let announceScene: () => void = () => undefined;
    let releaseScene: () => void = () => undefined;
    let announceCompare: () => void = () => undefined;
    let releaseCompare: () => void = () => undefined;
    const sceneCaptured = new Promise<void>((resolve) => { announceScene = resolve; });
    const sceneGate = new Promise<void>((resolve) => { releaseScene = resolve; });
    const compareCaptured = new Promise<void>((resolve) => { announceCompare = resolve; });
    const compareGate = new Promise<void>((resolve) => { releaseCompare = resolve; });
    let comparisonHeld = false;
    await page.route("**/api/v1/brain/context", async (route) => {
      const body = route.request().postDataJSON();
      // Delay actual daemon responses; keep the payload and graph evidence intact.
      const response = await route.fetch();
      if (body.token_budget === 2000) {
        announceScene();
        await sceneGate;
      } else if (firstContext === "loading" && !comparisonHeld) {
        comparisonHeld = true;
        announceCompare();
        await compareGate;
      }
      await route.fulfill({ response }).catch(() => undefined);
    });
    try {
      await page.goto("/");
      await page.getByTestId("search-input").fill("releaseA");
      await page.getByRole("listbox", { name: "Search results" }).getByRole("option", { name: /releaseA/ }).first().click();
      await sceneCaptured;
      await page.getByRole("tablist", { name: "Result representation" })
        .getByRole("tab", { name: "JSON representation", exact: true }).click();
      await page.getByRole("button", { name: "Compare", exact: true }).first().click();
      const dialog = page.getByRole("dialog", { name: "Compare context" });
      const input = dialog.getByRole("textbox", { name: "Second context seeds" });
      await expect(dialog).toBeVisible();
      if (firstContext === "loading") {
        await compareCaptured;
        await expect(input).toBeDisabled();
      } else {
        await expect(input).toBeEnabled();
      }
      const scene = page.getByRole("region", { name: "JSON result view" }).locator("pre");
      expect(JSON.parse(await scene.innerText()).graph.context_graph_meta).toBeFalsy();
      releaseScene();
      // Graph publication occurs after the initial request's symbol enrichment.
      // Waiting for that observable commit prevents a pre-response false pass.
      await expect.poll(async () => JSON.parse(await scene.innerText()).graph.context_graph_meta?.edge_scope).toBe("returned_nodes");
      const snapshot = JSON.parse(await scene.innerText());
      expect(snapshot.graph.nodes).toHaveLength(3);
      expect(snapshot.active_lens.label).toBe("Compare releaseA");
      expect(snapshot.analysis.diff.active).toBe(true);
      await expect(dialog).toBeVisible();
      releaseCompare();
      await expect(input).toBeEnabled();
      await input.fill("releaseC");
      await input.press("Enter");
      await expect(page.getByRole("region", { name: "Context comparison result" })).toBeVisible();
      expect(errors).toEqual([]);
    } finally {
      releaseScene();
      releaseCompare();
      await page.unrouteAll({ behavior: "wait" }).catch(() => undefined);
    }
  });
}

test("release cancelling a pending comparison cannot restore its late result", async ({ page }) => {
  const errors: string[] = [];
  page.on("pageerror", (error) => errors.push(error.message));
  await page.goto("/");
  await page.getByTestId("search-input").fill("releaseA");
  await page.getByRole("listbox", { name: "Search results" }).getByRole("option", { name: /releaseA/ }).first().click();
  await page.getByRole("button", { name: "Compare", exact: true }).first().click();
  const dialog = page.getByRole("dialog", { name: "Compare context" });
  const input = dialog.getByRole("textbox", { name: "Second context seeds" });
  await expect(input).toBeEnabled();
  let announce: () => void = () => undefined;
  let release: () => void = () => undefined;
  const captured = new Promise<void>((resolve) => { announce = resolve; });
  const gate = new Promise<void>((resolve) => { release = resolve; });
  await page.route("**/api/v1/brain/context", async (route) => {
    const response = await route.fetch();
    announce();
    await gate;
    await route.fulfill({ response }).catch(() => undefined);
  });
  try {
    await input.fill("releaseC");
    await input.press("Enter");
    await captured;
    await dialog.getByRole("button", { name: "Cancel" }).click();
    release();
    await expect(dialog).toBeHidden();
    await page.unrouteAll({ behavior: "wait" });
    await expect(page.getByRole("region", { name: "Context comparison result" })).toHaveCount(0);
    await page.getByRole("button", { name: "Compare", exact: true }).first().click();
    await expect(dialog.getByRole("textbox", { name: "Second context seeds" })).toBeEnabled();
    await dialog.getByRole("button", { name: "Cancel" }).click();
    expect(errors).toEqual([]);
  } finally {
    release();
  }
});
