import { expect, test, type Page } from "@playwright/test";

// nw-570: `?engine=wasm` used to download the ~37 MB snapshot four times
// (two independent hook instances, each with its own sync + load + refresh)
// and the URL sync then stripped `engine=wasm`, silently switching the page
// back to the server engine.

function countSnapshotRequests(page: Page) {
  const counter = { count: 0 };
  page.on("request", (request) => {
    if (new URL(request.url()).pathname === "/api/v1/snapshot.msgpack") {
      counter.count += 1;
    }
  });
  return counter;
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

test.describe("WASM engine (nw-570)", () => {
  test("?engine=wasm fetches the snapshot once, keeps the param, and shows the engine", async ({ page }) => {
    const snapshots = countSnapshotRequests(page);
    await openPanels(page, "/?engine=wasm");

    const badge = page.getByRole("status", { name: "Graph engine" });
    await expect(badge).toContainText(/WASM/);
    await expect(badge).toContainText(/ready/i, { timeout: 30_000 });

    // Drive a state change so the URL sync writes the address bar.
    await page.getByRole("group", { name: "Graph mode" }).getByRole("button", { name: /repos/i }).click();
    await expect.poll(() => new URL(page.url()).searchParams.get("mode")).toBe("repos");
    expect(new URL(page.url()).searchParams.get("engine")).toBe("wasm");

    // Give any duplicate loader time to fire before counting.
    await page.waitForTimeout(1_500);
    expect(snapshots.count).toBe(1);
    await expect(badge).toContainText(/ready/i);
  });

  test("counterweight: the default server engine never fetches the snapshot", async ({ page }) => {
    const snapshots = countSnapshotRequests(page);
    await openPanels(page, "/");
    await expect(page.getByRole("status", { name: "Graph engine" })).toContainText(/server/i);
    await page.getByRole("group", { name: "Graph mode" }).getByRole("button", { name: /repos/i }).click();
    await page.waitForTimeout(1_500);
    expect(snapshots.count).toBe(0);
    expect(new URL(page.url()).searchParams.get("engine")).toBeNull();
  });
});
