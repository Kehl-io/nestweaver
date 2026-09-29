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

/**
 * Route the SSE stream so a test can deliver one `graph:updated` on demand.
 * Each fulfilled response ends the stream; EventSource reconnects after
 * `retry`, and the next connection carries the queued event.
 */
async function controllableEvents(page: Page) {
  const control = { pending: false };
  await page.route("**/api/v1/events", (route) => {
    const body = control.pending
      ? "retry: 200\nevent: graph:updated\ndata: {}\n\n"
      : "retry: 200\n\n";
    control.pending = false;
    return route.fulfill({ status: 200, contentType: "text/event-stream", body });
  });
  return control;
}

/** The engine's completed sync passes (initial load plus each refresh check). */
async function engineSyncs(page: Page): Promise<number> {
  const value = await page.getByRole("status", { name: "Graph engine" }).getAttribute("data-syncs");
  return Number(value ?? "0");
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
    const events = await controllableEvents(page);
    const snapshots = countSnapshotRequests(page);
    await openPanels(page, "/?engine=wasm");

    const badge = page.getByRole("status", { name: "Graph engine" });
    await expect(badge).toContainText(/WASM/);
    await expect(badge).toContainText(/ready/i, { timeout: 30_000 });
    await expect.poll(() => engineSyncs(page)).toBe(1);
    expect(snapshots.count).toBe(1);

    // Drive a state change so the URL sync writes the address bar.
    await page.getByRole("group", { name: "Graph mode" }).getByRole("button", { name: /repos/i }).click();
    await expect.poll(() => new URL(page.url()).searchParams.get("mode")).toBe("repos");
    expect(new URL(page.url()).searchParams.get("engine")).toBe("wasm");

    // A graph update with an unchanged generation re-checks but does not
    // download again.
    events.pending = true;
    await expect.poll(() => engineSyncs(page)).toBe(2);
    expect(snapshots.count).toBe(1);
    await expect(badge).toContainText(/ready/i);
  });

  test("an unreadable /version never re-downloads a loaded snapshot on graph updates", async ({ page }) => {
    await page.route("**/api/v1/version", (route) =>
      route.fulfill({ status: 503, contentType: "application/json", body: '{"error":"unavailable"}' }),
    );
    const events = await controllableEvents(page);
    const snapshots = countSnapshotRequests(page);
    await openPanels(page, "/?engine=wasm");
    await expect(page.getByRole("status", { name: "Graph engine" })).toContainText(/ready/i, {
      timeout: 30_000,
    });
    await expect.poll(() => engineSyncs(page)).toBe(1);
    expect(snapshots.count).toBe(1);

    events.pending = true;
    await expect.poll(() => engineSyncs(page)).toBe(2);
    events.pending = true;
    await expect.poll(() => engineSyncs(page)).toBe(3);
    expect(snapshots.count).toBe(1);
  });

  test("counterweight: the default server engine never fetches the snapshot", async ({ page }) => {
    const events = await controllableEvents(page);
    const snapshots = countSnapshotRequests(page);
    await openPanels(page, "/");
    await expect(page.getByRole("status", { name: "Graph engine" })).toContainText(/server/i);
    await page.getByRole("group", { name: "Graph mode" }).getByRole("button", { name: /repos/i }).click();
    await expect.poll(() => new URL(page.url()).searchParams.get("mode")).toBe("repos");
    // Deliver a graph update and wait for the reconnect that follows it.
    events.pending = true;
    await page.waitForRequest((req) => new URL(req.url()).pathname === "/api/v1/events" && !events.pending);
    await page.waitForRequest((req) => new URL(req.url()).pathname === "/api/v1/events");
    expect(snapshots.count).toBe(0);
    expect(new URL(page.url()).searchParams.get("engine")).toBeNull();
  });
});
