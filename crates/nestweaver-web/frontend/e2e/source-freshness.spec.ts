import { execFile } from "node:child_process";
import { readFile, writeFile } from "node:fs/promises";
import { join } from "node:path";
import { promisify } from "node:util";
import { test, expect, type Page, type APIRequestContext, type TestInfo } from "@playwright/test";
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


interface OwnedSource {
  sourcePath: string;
  original: string;
  reindex: () => Promise<void>;
  find: () => Promise<{ uid: string; name: string; start_line: number }>;
}

async function withOwnedSource(request: APIRequestContext, testInfo: TestInfo, exercise: (source: OwnedSource) => Promise<void>) {
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
    await exercise({ sourcePath, original, reindex, find });
  } finally {
    await writeFile(sourcePath, original);
    await reindex();
  }
}

async function holdSymbolReply(page: Page, uid: string, status: number) {
  let holding = true;
  let count = 0;
  let releaseReply!: () => void;
  const gate = new Promise<void>((resolve) => { releaseReply = resolve; });
  await page.route("**/api/v1/symbol/**", async (route) => {
    const path = new URL(route.request().url()).pathname;
    if (decodeURIComponent(path.slice("/api/v1/symbol/".length)) !== uid) return route.continue();
    const response = await route.fetch();
    if (holding && response.status() === status) {
      count += 1;
      await gate;
    }
    await route.fulfill({ response });
  });
  return { count: () => count, release: () => { holding = false; releaseReply(); } };
}

function countSourceRequests(page: Page) {
  let count = 0;
  page.on("request", (request) => {
    const url = new URL(request.url());
    if (url.pathname === "/api/v1/source" && url.searchParams.get("file") === "release-journey.js") count += 1;
  });
  return () => count;
}

test("real body-only reindex waits for current metadata then refreshes both previews without reselection", async ({ page, request }, testInfo) => {
  test.setTimeout(180_000);
  await withOwnedSource(request, testInfo, async ({ sourcePath, original, reindex, find }) => {
    const requests = countSourceRequests(page);
    const before = await find();
    await selectSource(page);
    for (const preview of previews(page)) await expect(preview).toContainText("return releaseB();");
    const baseline = requests();
    const metadata = await holdSymbolReply(page, before.uid, 200);
    try {
      const changed = original.replace("return releaseB();", "return 'preview-after-real-index';");
      expect(changed).not.toBe(original);
      await writeFile(sourcePath, changed);
      await reindex();
      const after = await find();
      expect(after.uid).toBe(before.uid);
      expect(after.start_line).toBe(before.start_line);
      await expect.poll(metadata.count).toBeGreaterThan(0);
      for (const preview of previews(page)) await expect(preview).toHaveCount(0);
      expect(requests(), "new bodies wait for current successful symbol metadata").toBe(baseline);
      metadata.release();
      for (const preview of previews(page)) {
        await expect(preview).toContainText("preview-after-real-index", { timeout: 15_000 });
        await expect(preview).not.toContainText("return releaseB();");
      }
      expect(requests()).toBeGreaterThan(baseline);
      expect(new URL(page.url()).searchParams.get("node")).toBe(before.uid);
    } finally {
      metadata.release();
    }
  });
});

test("real moved-symbol reindex never attributes a new target line to held old metadata", async ({ page, request }, testInfo) => {
  test.setTimeout(180_000);
  await withOwnedSource(request, testInfo, async ({ sourcePath, original, reindex, find }) => {
    const requests = countSourceRequests(page);
    const before = await find();
    await selectSource(page);
    for (const preview of previews(page)) await expect(preview.locator("[aria-current=true]")).toContainText("function releaseA");
    const baseline = requests();
    const metadata = await holdSymbolReply(page, before.uid, 404);
    try {
      await writeFile(sourcePath, "export function unrelatedReplacement() { return 'UNRELATED_NEW_TARGET'; }\n" + original);
      await reindex();
      const after = await find();
      expect(after.uid).not.toBe(before.uid);
      expect(after.start_line).toBe(before.start_line + 1);
      const missing = await request.get(`/api/v1/symbol/${encodeURIComponent(before.uid)}`);
      expect(missing.status()).toBe(404);
      await expect.poll(metadata.count).toBeGreaterThan(0);
      for (const preview of previews(page)) await expect(preview).toHaveCount(0);
      expect(requests(), "invalidated symbol coordinates cannot request the new file body").toBe(baseline);
      expect(new URL(page.url()).searchParams.get("node")).toBe(before.uid);
      metadata.release();
      for (const panel of [page.getByTestId("detail-panel"), page.getByRole("complementary", { name: "Source and note evidence" })]) {
        await expect(panel.getByRole("heading", { name: "Node not found" })).toBeVisible();
        await expect(panel).not.toContainText("UNRELATED_NEW_TARGET");
      }
    } finally {
      metadata.release();
    }
  });
});


function fileDetails(page: Page) {
  const details = page.getByTestId("detail-panel");
  return { details, symbols: details.getByRole("list").getByRole("button") };
}

async function selectFile(page: Page) {
  await page.setViewportSize({ width: 1600, height: 1000 });
  await page.goto("/?node=release-journey.js&kind=file");
}

test("held file Details matches exact API symbols through real expansion and contraction", async ({ page, request }, testInfo) => {
  test.setTimeout(180_000);
  await withOwnedSource(request, testInfo, async ({ sourcePath, original, reindex }) => {
    const expectedNames = async () => {
      const source = await request.get("/api/v1/source?file=release-journey.js");
      expect(source.ok()).toBeTruthy();
      const owner = (await source.json() as { repo: string }).repo;
      const response = await request.get("/api/v1/symbols/file?path=release-journey.js");
      expect(response.ok()).toBeTruthy();
      return (await response.json() as { name: string; repo_uid: string }[])
        .filter((row) => row.repo_uid === owner).map((row) => row.name);
    };
    const assertMetadata = async () => {
      const names = await expectedNames();
      const { details, symbols } = fileDetails(page);
      await expect(symbols).toHaveCount(Math.min(names.length, 40));
      await expect(details.getByText(`${names.length} symbols`, { exact: true })).toBeVisible();
      for (const name of names.slice(0, 40)) await expect(symbols.getByText(name, { exact: true })).toBeVisible();
    };
    await selectFile(page);
    await assertMetadata();
    await writeFile(sourcePath, original + "export function metadataAddedAfterIndex() { return 'metadata-expansion'; }\n");
    await reindex();
    await assertMetadata();
    await expect(fileDetails(page).symbols.getByText("metadataAddedAfterIndex", { exact: true })).toBeVisible();
    await expect(page.getByTestId("detail-panel").getByRole("region", { name: "Source evidence for release-journey.js" }))
      .toContainText("metadataAddedAfterIndex");
    // Contraction is observed only after the expanded metadata really rendered.
    await writeFile(sourcePath, original);
    await reindex();
    await assertMetadata();
    await expect(fileDetails(page).symbols.getByText("metadataAddedAfterIndex", { exact: true })).toHaveCount(0);
    expect(new URL(page.url()).searchParams.get("node")).toBe("release-journey.js");
  });
});

interface MetadataFixture {
  uid: string;
  name: string;
  kind: string;
  file_path: string;
  repo_uid: string;
  start_line: number;
}

function metadataRows(repo: string, revision: string, count = 2): MetadataFixture[] {
  return Array.from({ length: count }, (_, i) => ({ uid: `sym:${repo}:${revision}:${i}`,
    name: `${revision}${i}`, kind: "Function", file_path: "release-journey.js", repo_uid: repo, start_line: i + 1 }));
}

async function uncancelledMetadataTransport(page: Page) {
  // Model a transport that finishes despite cancellation. The application's
  // signal/current-identity checks must still discard the decoded old reply.
  await page.addInitScript(() => {
    const nativeFetch = window.fetch.bind(window);
    const delivered = { count: 0, aborted: 0 };
    Object.assign(window, { metadataDelivery: delivered });
    window.fetch = async (input, init) => {
      if (!String(input).includes("/api/v1/symbols/file?")) return nativeFetch(input, init);
      init?.signal?.addEventListener("abort", () => { delivered.aborted += 1; });
      const response = await nativeFetch(input, { ...init, signal: undefined });
      delivered.count += 1;
      return response;
    };
  });
}

async function metadataDeliveryCount(page: Page) {
  return page.evaluate(() => (window as unknown as { metadataDelivery: { count: number } }).metadataDelivery.count);
}

test("file metadata rejects late older epochs and does not refetch for layout or rank changes", async ({ page }) => {
  await observeEvents(page);
  await uncancelledMetadataTransport(page);
  let revision = "initialMetadata";
  let metadataRequests = 0;
  let held = 0;
  let release!: () => void;
  const gate = new Promise<void>((resolve) => { release = resolve; });
  await page.route("**/api/v1/source?*", (route) => route.fulfill({ json: {
    file: "release-journey.js", repo: "repo:metadata", start_line: 1,
    lines: [`currentBody:${revision}`], total_lines: 1 } }));
  await page.route("**/api/v1/symbols/file?*", async (route) => {
    metadataRequests += 1;
    const captured = revision;
    if (captured === "obsoleteMetadata") { held += 1; await gate; }
    await route.fulfill({ json: metadataRows("repo:metadata", captured) });
  });
  try {
    await selectFile(page);
    const { details, symbols } = fileDetails(page);
    await expect(symbols.getByText("initialMetadata0", { exact: true })).toBeVisible();
    const baseline = metadataRequests;
    await emit(page, "watcher:status");
    await emit(page, "pagerank:recomputed");
    await page.getByRole("button", { name: "Settings", exact: true }).click();
    await page.getByRole("slider", { name: "Force Scale" }).fill("4.5");
    await page.waitForTimeout(650);
    expect(metadataRequests).toBe(baseline);
    revision = "obsoleteMetadata";
    await emit(page);
    await expect.poll(() => held).toBeGreaterThan(0);
    revision = "newestMetadata";
    await emit(page);
    await expect(symbols.getByText("newestMetadata0", { exact: true })).toBeVisible();
    const beforeRelease = await metadataDeliveryCount(page);
    release();
    // Observe fetch completion before the API's aborted-signal check, then
    // a painted frame: fulfilling the route alone is not a delivery barrier.
    await expect.poll(() => metadataDeliveryCount(page))
      .toBeGreaterThanOrEqual(beforeRelease + held);
    await renderedFrame(page);
    await expect(symbols.getByText("newestMetadata0", { exact: true })).toBeVisible();
    await expect(details).not.toContainText("obsoleteMetadata");
    expect(await page.evaluate(() => (window as unknown as { metadataDelivery: { aborted: number } }).metadataDelivery.aborted))
      .toBeGreaterThan(0);
  } finally { release(); }
});

test("file owner picks survive valid commits and are requalified when indexed ownership changes", async ({ page }) => {
  await observeEvents(page);
  const owners = ["repo:metadata-a", "repo:metadata-b"];
  let available = owners;
  let revision = "ownerBefore";
  await page.route("**/api/v1/repos", (route) => route.fulfill({ json: owners.map((uid) => ({ uid, name: uid, url: uid })) }));
  await page.route("**/api/v1/source?*", (route) => {
    const selected = new URL(route.request().url()).searchParams.get("repo");
    if (!selected && available.length > 1) return route.fulfill({ status: 409, json: {
      error: "ambiguous_file", candidates: available, message: "choose owner" } });
    const repo = selected ?? available[0];
    if (!available.includes(repo)) return route.fulfill({ status: 404, json: { error: "source_not_indexed" } });
    return route.fulfill({ json: { file: "release-journey.js", repo, start_line: 1,
      lines: [`${repo}:${revision}`], total_lines: 1 } });
  });
  await page.route("**/api/v1/symbols/file?*", (route) => route.fulfill({ json:
    available.flatMap((repo) => metadataRows(repo, `${repo === owners[0] ? "OwnerA" : "OwnerB"}${revision}`)) }));
  await selectFile(page);
  const { details, symbols } = fileDetails(page);
  const picker = details.getByRole("group", { name: "Repos containing release-journey.js" });
  await expect(picker).toBeVisible();
  await expect(symbols).toHaveCount(0);
  const pick = picker.getByRole("button", { name: "Show release-journey.js from repo:metadata-a", exact: true });
  await pick.click();
  await expect(symbols.getByText("OwnerAownerBefore0", { exact: true })).toBeVisible();
  await expect(symbols).toHaveCount(2);
  await pick.focus();
  revision = "ownerAfter";
  await emit(page);
  await expect(symbols.getByText("OwnerAownerAfter0", { exact: true })).toBeVisible();
  await expect(pick).toBeFocused();
  await expect(pick).toHaveAttribute("aria-pressed", "true");
  await expect(details).not.toContainText("OwnerBownerAfter");
  available = [owners[1]];
  await emit(page);
  await expect(symbols.getByText("OwnerBownerAfter0", { exact: true })).toBeVisible();
  await expect(details.getByText("2 symbols", { exact: true })).toBeVisible();
  await expect(details).not.toContainText("OwnerAownerAfter");
  await expect(picker).toHaveCount(0);
});

test("failed file metadata never claims an empty successful symbol count", async ({ page }) => {
  await observeEvents(page);
  let fail = false;
  await page.route("**/api/v1/source?*", (route) => route.fulfill({ json: {
    file: "release-journey.js", repo: "repo:metadata", start_line: 1, lines: ["metadata source"], total_lines: 1 } }));
  await page.route("**/api/v1/symbols/file?*", (route) => fail
    ? route.fulfill({ status: 503, json: { error: "metadata_unavailable", message: "Metadata read unavailable" } })
    : route.fulfill({ json: metadataRows("repo:metadata", "beforeFailure") }));
  await selectFile(page);
  const { details, symbols } = fileDetails(page);
  await expect(symbols.getByText("beforeFailure0", { exact: true })).toBeVisible();
  fail = true;
  await emit(page);
  await expect(details).toContainText("Metadata read unavailable");
  await expect(details.getByText("0 symbols", { exact: true })).toHaveCount(0);
  await expect(symbols).toHaveCount(0);
  fail = false;
  await emit(page);
  await expect(symbols.getByText("beforeFailure0", { exact: true })).toBeVisible();
});


function fileWorkspace(id: string, type: "all" | "repo", label: string) {
  const counts = { project_count: 0, repo_count: 1, service_count: 0,
    vault_count: 0, note_count: 0, symbol_count: 2 };
  const _meta = { workspace_id: id, workspace_type: type,
    trust: { data_scope: "local-only", federation: "local-only", freshness: "current", capability: "local-index",
      result: "complete", source_confidence: "extracted", partial: false, unsupported: [], message: "" },
    provenance: [{ source: "fixture", detail: "file identity fixture" }],
    truncation: { truncated: false }, continuation: { has_more: false } };
  return { id, type, label, uid: id, counts, _meta };
}

test("workspace and path switches cannot commit old file metadata", async ({ page }) => {
  await observeEvents(page);
  await uncancelledMetadataTransport(page);
  const workspaces = [fileWorkspace("all", "all", "Metadata all"), fileWorkspace("repo:metadata", "repo", "Metadata scoped")];
  await page.route("**/api/v1/workspaces", (route) => route.fulfill({ json: { workspaces, _meta: workspaces[0]._meta } }));
  let revision = "scopeBefore";
  let held = 0;
  let release!: () => void;
  const gate = new Promise<void>((resolve) => { release = resolve; });
  await page.route("**/api/v1/source?*", (route) => route.fulfill({ json: {
    file: new URL(route.request().url()).searchParams.get("file"), repo: "repo:metadata",
    start_line: 1, lines: [`body:${revision}`], total_lines: 1 } }));
  await page.route("**/api/v1/symbols/file?*", async (route) => {
    const captured = revision;
    const path = new URL(route.request().url()).searchParams.get("path") ?? "";
    if (captured === "scopeDelayed" || captured === "pathDelayed") { held += 1; await gate; }
    await route.fulfill({ json: metadataRows("repo:metadata", captured).map((row) => ({ ...row, file_path: path })) });
  });
  try {
    await selectFile(page);
    const { details, symbols } = fileDetails(page);
    await expect(symbols.getByText("scopeBefore0", { exact: true })).toBeVisible();
    revision = "scopeDelayed";
    await emit(page);
    await expect.poll(() => held).toBeGreaterThan(0);
    revision = "scopeCurrent";
    await page.getByLabel("Workspace", { exact: true }).click();
    await page.getByRole("option", { name: /Metadata scoped/ }).click();
    await expect(symbols.getByText("scopeCurrent0", { exact: true })).toBeVisible();
    // Keep the old scope response parked while a second identity (path) changes.
    revision = "pathDelayed";
    const oldHeld = held;
    await emit(page);
    await expect.poll(() => held).toBeGreaterThan(oldHeld);
    revision = "pathCurrent";
    await page.evaluate(() => {
      const url = new URL(window.location.href);
      url.searchParams.set("node", "metadata-other.js");
      url.searchParams.set("kind", "file");
      window.history.pushState(null, "", url);
      window.dispatchEvent(new PopStateEvent("popstate"));
    });
    await expect(details.getByText("metadata-other.js", { exact: true }).first()).toBeVisible();
    await expect(symbols.getByText("pathCurrent0", { exact: true })).toBeVisible();
    const delivered = await metadataDeliveryCount(page);
    release();
    await expect.poll(() => metadataDeliveryCount(page)).toBeGreaterThanOrEqual(delivered + held);
    await renderedFrame(page);
    await expect(symbols.getByText("pathCurrent0", { exact: true })).toBeVisible();
    await expect(details).not.toContainText("scopeDelayed");
    await expect(details).not.toContainText("pathDelayed");
    expect(new URL(page.url()).searchParams.get("workspace")).toBe("repo:metadata");
  } finally { release(); }
});

test("switching a retained file owner discards its delayed metadata response", async ({ page }) => {
  await observeEvents(page);
  await uncancelledMetadataTransport(page);
  const owners = ["repo:metadata-a", "repo:metadata-b"];
  let revision = "pickBefore";
  let held = 0;
  let release!: () => void;
  const gate = new Promise<void>((resolve) => { release = resolve; });
  await page.route("**/api/v1/repos", (route) => route.fulfill({ json: owners.map((uid) => ({ uid, name: uid, url: uid })) }));
  await page.route("**/api/v1/source?*", (route) => {
    const repo = new URL(route.request().url()).searchParams.get("repo");
    return repo ? route.fulfill({ json: { file: "release-journey.js", repo, start_line: 1,
      lines: [`body:${repo}:${revision}`], total_lines: 1 } }) : route.fulfill({ status: 409,
      json: { error: "ambiguous_file", candidates: owners, message: "choose owner" } });
  });
  await page.route("**/api/v1/symbols/file?*", async (route) => {
    const captured = revision;
    if (captured === "pickDelayed") { held += 1; await gate; }
    await route.fulfill({ json: owners.flatMap((owner, i) => metadataRows(owner, `Owner${i}${captured}`)) });
  });
  try {
    await selectFile(page);
    const { details, symbols } = fileDetails(page);
    const picker = details.getByRole("group", { name: "Repos containing release-journey.js" });
    await picker.getByRole("button", { name: "Show release-journey.js from repo:metadata-a", exact: true }).click();
    await expect(symbols.getByText("Owner0pickBefore0", { exact: true })).toBeVisible();
    revision = "pickDelayed";
    await emit(page);
    await expect.poll(() => held).toBeGreaterThan(0);
    revision = "pickCurrent";
    await picker.getByRole("button", { name: "Show release-journey.js from repo:metadata-b", exact: true }).click();
    await expect(symbols.getByText("Owner1pickCurrent0", { exact: true })).toBeVisible();
    const delivered = await metadataDeliveryCount(page);
    release();
    await expect.poll(() => metadataDeliveryCount(page)).toBeGreaterThanOrEqual(delivered + held);
    await renderedFrame(page);
    await expect(symbols.getByText("Owner1pickCurrent0", { exact: true })).toBeVisible();
    await expect(details).not.toContainText("pickDelayed");
    await expect(picker.getByRole("button", { name: "Show release-journey.js from repo:metadata-b", exact: true }))
      .toHaveAttribute("aria-pressed", "true");
  } finally { release(); }
});
