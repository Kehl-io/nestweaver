import {
  expect,
  test,
  type APIRequestContext,
  type APIResponse,
  type Locator,
  type Page,
} from "@playwright/test";

interface CanvasPixelStats {
  cssWidth: number;
  cssHeight: number;
  bitmapWidth: number;
  bitmapHeight: number;
  distinctBuckets: number;
  colorSpread: number;
  signalPixels: number;
  signalRatio: number;
}

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

async function waitForOverview(request: APIRequestContext): Promise<void> {
  const response = await getOk(request, "/api/v1/overview?limit=24");
  const overview = await response.json();
  expect(overview).toHaveProperty("counts");
  expect(Array.isArray(overview.landmarks)).toBeTruthy();
  expect(Array.isArray(overview.start_here)).toBeTruthy();
  expect(Array.isArray(overview.gaps)).toBeTruthy();
}

async function openGraph(page: Page, request: APIRequestContext): Promise<void> {
  await waitForOverview(request);
  await page.goto("/");

  await expect(page.getByTestId("graph-panel")).toBeVisible({
    timeout: 15_000,
  });
  await expect(
    page.getByRole("application", { name: "Code knowledge graph" }),
  ).toBeVisible();
  await expect(page.getByTestId("control-dock")).toBeVisible();
}

async function openShortcutsDialog(page: Page): Promise<Locator> {
  const shortcutsDialog = page.getByRole("dialog", {
    name: "Keyboard Shortcuts",
  });

  await page.keyboard.press("?");
  return shortcutsDialog;
}

async function focusBody(page: Page): Promise<void> {
  await page.evaluate(() => {
    document.body.setAttribute("tabindex", "-1");
    document.body.focus();
    document.body.removeAttribute("tabindex");
  });

  await expect
    .poll(
      async () => page.evaluate(() => document.activeElement === document.body),
      { message: "body should receive focus before opening fallback dialog" },
    )
    .toBe(true);
}

async function canvasPixelStats(canvas: Locator): Promise<CanvasPixelStats> {
  return canvas.evaluate((element) => {
    const source = element as HTMLCanvasElement;
    const rect = source.getBoundingClientRect();
    const bitmapWidth = source.width;
    const bitmapHeight = source.height;
    const sampleWidth = Math.min(
      160,
      Math.max(1, bitmapWidth || Math.round(rect.width)),
    );
    const sampleHeight = Math.min(
      120,
      Math.max(1, bitmapHeight || Math.round(rect.height)),
    );
    const scratch = document.createElement("canvas");
    scratch.width = sampleWidth;
    scratch.height = sampleHeight;
    const context = scratch.getContext("2d", { willReadFrequently: true });

    if (!context) {
      throw new Error("2D canvas context unavailable for pixel check");
    }

    context.drawImage(source, 0, 0, sampleWidth, sampleHeight);

    const data = context.getImageData(0, 0, sampleWidth, sampleHeight).data;
    const buckets = new Map<string, number>();
    let minR = 255;
    let minG = 255;
    let minB = 255;
    let maxR = 0;
    let maxG = 0;
    let maxB = 0;
    let sampleCount = 0;

    for (let index = 0; index < data.length; index += 4) {
      const alpha = data[index + 3];
      if (alpha === 0) continue;

      const red = data[index];
      const green = data[index + 1];
      const blue = data[index + 2];
      minR = Math.min(minR, red);
      minG = Math.min(minG, green);
      minB = Math.min(minB, blue);
      maxR = Math.max(maxR, red);
      maxG = Math.max(maxG, green);
      maxB = Math.max(maxB, blue);
      sampleCount += 1;

      const bucket = `${red >> 4}:${green >> 4}:${blue >> 4}`;
      buckets.set(bucket, (buckets.get(bucket) ?? 0) + 1);
    }

    const dominantBucketCount = Math.max(0, ...buckets.values());
    const signalPixels = Math.max(0, sampleCount - dominantBucketCount);

    return {
      cssWidth: rect.width,
      cssHeight: rect.height,
      bitmapWidth,
      bitmapHeight,
      distinctBuckets: buckets.size,
      colorSpread: maxR - minR + (maxG - minG) + (maxB - minB),
      signalPixels,
      signalRatio: sampleCount > 0 ? signalPixels / sampleCount : 0,
    };
  });
}

function canvasSignalDiagnostic(stats: CanvasPixelStats): string {
  const hasSize =
    stats.cssWidth > 0 &&
    stats.cssHeight > 0 &&
    stats.bitmapWidth > 0 &&
    stats.bitmapHeight > 0;
  const hasSignal =
    stats.distinctBuckets >= 4 &&
    stats.colorSpread >= 30 &&
    stats.signalPixels >= 50 &&
    stats.signalRatio >= 0.002;

  return hasSize && hasSignal ? "ready" : JSON.stringify(stats);
}

test.describe("P0 foundation release gates", () => {
  test("renders a framed, nonblank graph canvas", async ({ page, request }) => {
    await openGraph(page, request);

    const graphPanel = page.getByTestId("graph-panel");
    const canvas = graphPanel.locator("canvas").first();
    await expect(canvas).toBeVisible({ timeout: 15_000 });

    const box = await canvas.boundingBox();
    expect(box).not.toBeNull();
    expect(box?.width).toBeGreaterThan(0);
    expect(box?.height).toBeGreaterThan(0);

    await expect
      .poll(async () => canvasSignalDiagnostic(await canvasPixelStats(canvas)), {
        message: "graph canvas should contain non-background pixel signal",
        timeout: 15_000,
      })
      .toBe("ready");
  });

  test("mode tabs are keyboard reachable and update pressed state", async ({
    page,
    request,
  }) => {
    await openGraph(page, request);

    const group = page.getByRole("group", { name: "Graph mode" });
    await expect(group).toBeVisible();

    const overview = group.getByRole("button", { name: "Overview" });
    const context = group.getByRole("button", { name: "Context" });
    const modes = ["Impact", "Repos", "Features", "Local"];

    await expect(overview).toHaveAttribute("aria-pressed", "true");
    await overview.focus();
    await expect(overview).toBeFocused();
    await page.keyboard.press("Tab");
    await expect(context).toBeFocused();

    await page.keyboard.press("Enter");
    await expect(context).toHaveAttribute("aria-pressed", "true");
    await expect(overview).toHaveAttribute("aria-pressed", "false");

    for (const mode of modes) {
      await expect(group.getByRole("button", { name: mode })).toBeVisible();
    }
  });

  test("global query dialog opens and closes from keyboard", async ({
    page,
    request,
  }) => {
    await openGraph(page, request);

    const searchInput = page.getByTestId("search-input");
    await searchInput.focus();
    await expect(searchInput).toBeFocused();

    const askShortcut = process.platform === "darwin" ? "Meta+K" : "Control+K";
    await page.keyboard.press(askShortcut);

    const askDialog = page.getByRole("dialog", { name: "Ask" });
    await expect(askDialog).toBeVisible();
    await expect(
      askDialog.getByText("Natural language to PPR subgraph"),
    ).toBeVisible();
    await expect(askDialog.getByLabel("Ask")).toBeFocused();

    await page.keyboard.press("Escape");
    await expect(askDialog).toHaveCount(0);
    await expect(searchInput).toBeFocused();
  });

  test("question mark opens shortcuts overlay and Escape closes it", async ({
    page,
    request,
  }) => {
    await openGraph(page, request);

    const settingsButton = page
      .getByTestId("control-dock")
      .getByRole("button", { name: "Settings" });
    await settingsButton.focus();
    await expect(settingsButton).toBeFocused();

    const shortcutsDialog = await openShortcutsDialog(page);
    await expect(shortcutsDialog).toBeVisible();

    const closeButton = shortcutsDialog.getByRole("button", {
      name: "Close keyboard shortcuts",
    });
    await expect(closeButton).toBeVisible();
    await page.keyboard.press("Tab");
    await expect(closeButton).toBeFocused();

    await page.keyboard.press("Escape");
    await expect(shortcutsDialog).toHaveCount(0);
    await expect(settingsButton).toBeFocused();
  });

  test("dialogs fall back to the current graph panel application in list view", async ({
    page,
    request,
  }) => {
    await openGraph(page, request);

    const graphApp = page.getByRole("application", {
      name: "Code knowledge graph",
    });
    await graphApp.focus();

    const viewShortcut = process.platform === "darwin" ? "Meta+L" : "Control+L";
    await page.keyboard.press(viewShortcut);

    const listApp = page.getByRole("application", { name: "Node list view" });
    await expect(listApp).toBeVisible();

    await focusBody(page);

    const askShortcut = process.platform === "darwin" ? "Meta+K" : "Control+K";
    await page.keyboard.press(askShortcut);

    const askDialog = page.getByRole("dialog", { name: "Ask" });
    await expect(askDialog).toBeVisible();
    await page.keyboard.press("Escape");
    await expect(askDialog).toHaveCount(0);
    await expect(listApp).toBeFocused();

    await focusBody(page);

    const shortcutsDialog = await openShortcutsDialog(page);
    await expect(shortcutsDialog).toBeVisible();
    await page.keyboard.press("Escape");
    await expect(shortcutsDialog).toHaveCount(0);
    await expect(listApp).toBeFocused();
  });

  test("reduced effects is accessible and toggles in reduced-motion contexts", async ({
    page,
    request,
  }) => {
    await page.emulateMedia({ reducedMotion: "reduce" });
    await openGraph(page, request);

    const dock = page.getByTestId("control-dock");
    await dock.getByRole("button", { name: "Settings" }).click();

    const reducedEffects = dock.getByRole("button", {
      name: /Reduced effects/,
    });
    await expect(reducedEffects).toBeVisible();
    await expect(reducedEffects).toHaveAttribute("aria-pressed", /true|false/);

    const initialState = await reducedEffects.getAttribute("aria-pressed");
    expect(initialState === "true" || initialState === "false").toBeTruthy();

    await reducedEffects.click();
    await expect(reducedEffects).toHaveAttribute(
      "aria-pressed",
      initialState === "true" ? "false" : "true",
    );
  });

  test("gap analysis failures surface a notification", async ({
    page,
    request,
  }) => {
    await page.route("**/api/v1/gaps", async (route) => {
      await route.fulfill({
        status: 500,
        contentType: "application/json",
        body: JSON.stringify({ error: "P0 gap failure probe" }),
      });
    });

    await openGraph(page, request);

    const dock = page.getByTestId("control-dock");
    await dock.getByRole("button", { name: "Settings" }).click();
    await dock.getByRole("button", { name: "Gaps" }).click();

    const notifications = page.getByRole("region", { name: "Notifications" });
    await expect(notifications).toBeVisible();
    await expect(
      notifications.getByText("Gap analysis failed", { exact: true }),
    ).toBeVisible();
  });
});
