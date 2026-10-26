import { execFile } from "node:child_process";
import { readFile, writeFile } from "node:fs/promises";
import { join } from "node:path";
import { promisify } from "node:util";
import { test, expect, type Page } from "@playwright/test";
import { renderedFrame } from "./deliveryBarriers";

// Keep native SSE and real symbol/repository metadata. Only source payloads
// and event delivery are controlled, so stable source coordinates are explicit.
async function observeEvents(page: Page) {
  await page.addInitScript(() => {
    const sources = new Set<EventSource>();
    const NativeEventSource = window.EventSource;
    window.EventSource = class extends NativeEventSource {
      constructor(url: string | URL, options?: EventSourceInit) {
        super(url, options);
        sources.add(this);
      }
      close() {
        sources.delete(this);
        super.close();
      }
    };
    Object.assign(window, {
      emitSourceEvent(type: string) {
        for (const source of sources) source.dispatchEvent(new MessageEvent(type, { data: "{}" }));
      },
    });
  });
}

async function emit(page: Page, type = "graph:updated") {
  await page.evaluate((type) => {
    (window as unknown as { emitSourceEvent: (type: string) => void }).emitSourceEvent(type);
  }, type);
}

async function selectSource(page: Page) {
  await page.setViewportSize({ width: 1600, height: 1000 });
  await page.goto("/");
  await page.getByTestId("search-input").fill("releaseA");
  await page.getByRole("listbox", { name: "Search results" })
    .getByRole("option", { name: /releaseA/ }).first().click();
}

function previews(page: Page) {
  return [
    page.getByTestId("detail-panel").getByRole("region", { name: "Source evidence for releaseA" }),
    page.getByRole("complementary", { name: "Source and note evidence" })
      .getByRole("region", { name: /Source preview for release-journey\.js:1/ }),
  ];
}

test("source previews refresh stable coordinates only after committed graph updates", async ({ page }) => {
  await observeEvents(page);
  let revision = "source-before-index";
  let requests = 0;
  const owners: string[] = [];
  await page.route("**/api/v1/source?*", async (route) => {
    requests += 1;
    owners.push(new URL(route.request().url()).searchParams.get("repo") ?? "");
    const response = await route.fetch();
    const source = await response.json();
    await route.fulfill({ response, json: { ...source, lines: [revision] } });
  });
  await selectSource(page);
  for (const preview of previews(page)) await expect(preview).toContainText(revision);
  const baseline = requests;
  revision = "source-after-index";
  // Neither a heartbeat nor a rank-only change commits source changes.
  await emit(page, "watcher:status");
  await emit(page, "pagerank:recomputed");
  await page.getByRole("button", { name: "Settings", exact: true }).click();
  await page.getByRole("slider", { name: "Force Scale" }).fill("4.5");
  await renderedFrame(page);
  // The real SSE coalescer has a 400ms quiet window.
  await page.waitForTimeout(650);
  expect(requests).toBe(baseline);
  for (const preview of previews(page)) await expect(preview).toContainText("source-before-index");
  await emit(page);
  for (const preview of previews(page)) await expect(preview).toContainText(revision);
  expect(requests).toBeGreaterThan(baseline);
  expect(owners.every((owner) => owner.length > 0)).toBeTruthy();
});

test("late source replies from an older committed epoch cannot replace newer previews", async ({ page }) => {
  await observeEvents(page);
  let revision = "initial-source";
  let held = 0;
  let released = 0;
  let releaseOld!: () => void;
  const oldReplyGate = new Promise<void>((resolve) => { releaseOld = resolve; });
  await page.route("**/api/v1/source?*", async (route) => {
    const capturedRevision = revision;
    const response = await route.fetch();
    const source = await response.json();
    if (capturedRevision === "delayed-old-source") {
      held += 1;
      await oldReplyGate;
    }
    await route.fulfill({ response, json: { ...source, lines: [capturedRevision] } });
    if (capturedRevision === "delayed-old-source") released += 1;
  });
  try {
    await selectSource(page);
    for (const preview of previews(page)) await expect(preview).toContainText(revision);
    revision = "delayed-old-source";
    await emit(page);
    await expect.poll(() => held).toBeGreaterThanOrEqual(2);
    revision = "newest-source";
    await emit(page);
    for (const preview of previews(page)) await expect(preview).toContainText(revision);
    releaseOld();
    await expect.poll(() => released).toBe(held);
    await renderedFrame(page);
    for (const preview of previews(page)) {
      await expect(preview).toContainText("newest-source");
      await expect(preview).not.toContainText("delayed-old-source");
    }
  } finally {
    releaseOld();
  }
});


test("committed source removal clears old bodies and preserves typed missing-source disclosure", async ({ page }) => {
  await observeEvents(page);
  let missing = false;
  await page.route("**/api/v1/source?*", async (route) => {
    if (missing) {
      await route.fulfill({ status: 404, json: { error: "source_not_indexed", message: "source removed" } });
    } else {
      const response = await route.fetch();
      const source = await response.json();
      await route.fulfill({ response, json: { ...source, lines: ["body-before-removal"] } });
    }
  });
  await selectSource(page);
  for (const preview of previews(page)) await expect(preview).toContainText("body-before-removal");
  missing = true;
  await emit(page);
  for (const panel of [page.getByTestId("detail-panel"), page.getByRole("complementary", { name: "Source and note evidence" })]) {
    await expect(panel).toContainText("Source not available: release-journey.js is not indexed.");
    await expect(panel).not.toContainText("body-before-removal");
  }
});


test("real body-only reindex refreshes both selected source previews without reselection", async ({ page, request }, testInfo) => {
  test.setTimeout(180_000);
  const fixture = JSON.parse(process.env.NESTWEAVER_UI_SOURCE_FIXTURE ?? "null") as {
    binary: string; repo: string; db: string; pidfile: string; pid: number;
  } | null;
  expect(fixture, "owned release_ui.py fixture is required").not.toBeNull();
  if (!fixture) throw new Error("missing owned source fixture");
  const sourcePath = join(fixture.repo, "release-journey.js");
  const original = await readFile(sourcePath, "utf8");
  const run = promisify(execFile);
  const reindex = async () => {
    expect(Number(await readFile(fixture.pidfile, "utf8"))).toBe(fixture.pid);
    const result = await run(fixture.binary, ["index", "--repo", fixture.repo, "--db", fixture.db, "--json"], { timeout: 120_000 });
    await testInfo.attach("source-reindex.json", { body: JSON.stringify(result), contentType: "application/json" });
    expect(Number(await readFile(fixture.pidfile, "utf8"))).toBe(fixture.pid);
  };
  const find = async () => {
    const response = await request.get("/api/v1/search?q=releaseA");
    expect(response.ok()).toBeTruthy();
    const symbols = await response.json() as { uid: string; name: string; start_line: number }[];
    const symbol = symbols.find((symbol) => symbol.name === "releaseA");
    expect(symbol).toBeTruthy();
    return symbol!;
  };
  try {
    const before = await find();
    await selectSource(page);
    for (const preview of previews(page)) await expect(preview).toContainText("return releaseB();");
    const changed = original.replace("return releaseB();", "return 'preview-after-real-index';");
    expect(changed).not.toBe(original);
    await writeFile(sourcePath, changed);
    await reindex();
    const after = await find();
    expect(after.uid).toBe(before.uid);
    expect(after.start_line).toBe(before.start_line);
    for (const preview of previews(page)) {
      await expect(preview).toContainText("preview-after-real-index", { timeout: 15_000 });
      await expect(preview).not.toContainText("return releaseB();");
    }
    expect(new URL(page.url()).searchParams.get("node")).toBe(before.uid);
  } finally {
    await writeFile(sourcePath, original);
    await reindex();
  }
});
