import { expect, test, type APIRequestContext, type Page } from "@playwright/test";

// nw-572: choosing a Start Here repo left the shelf on the canvas (with its
// Explore/Impact greyed out), popped a second repo card, never fitted the
// graph to the choice, and Details showed the raw `repo:` uid.

interface Landmark {
  uid: string;
  kind: string;
  label: string;
}

async function overview(request: APIRequestContext) {
  for (let attempt = 0; attempt < 40; attempt += 1) {
    const response = await request.get("/api/v1/overview?limit=96");
    if (response.ok()) {
      return (await response.json()) as { landmarks: Landmark[]; start_here: Landmark[] };
    }
    await new Promise((resolve) => setTimeout(resolve, 250));
  }
  throw new Error("overview never became ready");
}

async function openPanels(page: Page, path: string) {
  await page.setViewportSize({ width: 1440, height: 900 });
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

function cameraFit(page: Page) {
  return page.getByTestId("graph-panel").locator("canvas").first().getAttribute("data-camera-fit");
}

test.describe("Start Here (nw-572)", () => {
  test("choosing a Start Here entry dismisses the shelf and fits the graph to it", async ({
    page,
    request,
  }) => {
    const data = await overview(request);
    const entry = data.start_here[0];
    expect(entry, "fixture overview has a Start Here entry").toBeTruthy();
    await openPanels(page, "/");

    const shelf = page.getByRole("region", { name: "Start Here" });
    await expect(shelf).toBeVisible();
    await shelf.getByRole("button", { name: new RegExp(entry.label) }).first().click();

    await expect(shelf).toHaveCount(0);
    await expect.poll(() => cameraFit(page)).toBe(entry.uid);
    // Exactly one card describes the choice.
    await expect(page.getByRole("complementary", { name: "Overview context" })).toHaveCount(1);

    // Clearing the selection brings Start Here back.
    await page
      .getByRole("complementary", { name: "Overview context" })
      .getByRole("button", { name: "Back to Start Here" })
      .click();
    await expect(shelf).toBeVisible();
  });

  test("a repo deep link shows the repo name in Details and fits the graph to it", async ({
    page,
    request,
  }) => {
    const data = await overview(request);
    const repo = data.landmarks.find((landmark) => landmark.kind === "repo");
    expect(repo, "fixture overview has a repo landmark").toBeTruthy();
    await openPanels(page, `/?node=${encodeURIComponent(repo!.uid)}&kind=repo`);

    await expect(page.getByRole("region", { name: "Start Here" })).toHaveCount(0);
    const details = page.getByTestId("detail-panel");
    await expect(details.getByRole("heading", { name: repo!.label })).toBeVisible();
    await expect(details.getByRole("heading", { name: repo!.uid })).toHaveCount(0);
    await expect.poll(() => cameraFit(page)).toBe(repo!.uid);
  });

  test("selecting another node after a targeted fit keeps the camera where it is", async ({
    page,
    request,
  }) => {
    const data = await overview(request);
    const repo = data.landmarks.find((landmark) => landmark.kind === "repo")!;
    await openPanels(page, `/?node=${encodeURIComponent(repo.uid)}&kind=repo`);
    await expect.poll(() => cameraFit(page)).toBe(repo.uid);

    // Ctrl+Tab cycles the selection through the graph without a new fit.
    await page.getByRole("application", { name: "Code knowledge graph" }).focus();
    await page.keyboard.press("Control+Tab");
    await expect.poll(() => new URL(page.url()).searchParams.get("node")).not.toBe(repo.uid);
    await page.keyboard.press("Control+Tab");
    // Two selection changes and two frames later, still the repo's fit.
    await page.evaluate(() => new Promise((r) => requestAnimationFrame(() => requestAnimationFrame(r))));
    expect(await cameraFit(page)).toBe(repo.uid);
  });

  test("deselecting drops the fit target, so the next refit frames everything", async ({
    page,
    request,
  }) => {
    const data = await overview(request);
    const entry = data.start_here[0];
    await openPanels(page, "/");
    const shelf = page.getByRole("region", { name: "Start Here" });
    await shelf.getByRole("button", { name: new RegExp(entry.label) }).first().click();
    await expect.poll(() => cameraFit(page)).toBe(entry.uid);

    await page
      .getByRole("complementary", { name: "Overview context" })
      .getByRole("button", { name: "Back to Start Here" })
      .click();
    await expect(shelf).toBeVisible();
    // A resize refits the camera.
    await page.setViewportSize({ width: 1300, height: 860 });
    await expect.poll(() => cameraFit(page)).toBe("all");
  });

  test("switching workspace drops the fit target", async ({ page, request }) => {
    const data = await overview(request);
    const entry = data.start_here[0];
    const catalog = (await (await request.get("/api/v1/workspaces")).json()) as {
      workspaces: { id: string; type: string; label: string }[];
    };
    const workspace = catalog.workspaces.find((w) => w.type === "repo");
    expect(workspace, "fixture has a repo workspace").toBeTruthy();
    await openPanels(page, "/");
    await page
      .getByRole("region", { name: "Start Here" })
      .getByRole("button", { name: new RegExp(entry.label) })
      .first()
      .click();
    await expect.poll(() => cameraFit(page)).toBe(entry.uid);

    await page.getByLabel("Workspace").click();
    await page.getByRole("option", { name: new RegExp(workspace!.label) }).click();
    await expect(page.getByTestId("status-bar")).toContainText(workspace!.label);
    await expect.poll(() => cameraFit(page)).toBe("all");
  });

  test("counterweight: Start Here still shows on a fresh / with no node", async ({ page }) => {
    await openPanels(page, "/");
    await expect(page.getByRole("region", { name: "Start Here" })).toBeVisible();
    await expect(page.getByRole("complementary", { name: "Overview context" })).toHaveCount(0);
  });
});
