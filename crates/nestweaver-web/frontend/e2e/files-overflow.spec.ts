import { writeFile } from "node:fs/promises";
import { expect, test, type APIRequestContext, type Locator, type Page, type TestInfo } from "@playwright/test";
import type { SymbolCandidate } from "../src/api/types";
import { decodedReplyCount, eventCount, installDeliveryReceipts, renderedFrame, waitForDecodedReply } from "./deliveryBarriers";

const FOLDER = "ui-folder/overflow-folder-with-a-deliberately-long-name-that-wraps-in-the-explorer";
const FOLDER_TITLE = `Show files in ${FOLDER}; arrow keys expand or collapse`;

async function openOverflowFiles(page: Page, request: APIRequestContext) {
  // This is a required fixture precondition, never an optional/skip condition.
  const response = await request.get("/api/v1/symbols/top?limit=500");
  expect(response.ok()).toBe(true);
  const symbols = await response.json() as SymbolCandidate[];
  expect(symbols.length, "the real fixture fits below the Files symbol cap").toBeLessThan(500);
  const files = symbols.filter((symbol) => symbol.file_path.startsWith(`${FOLDER}/`));
  expect(files.map((symbol) => symbol.file_path).sort()).toEqual(
    Array.from({ length: 64 }, (_, i) => `${FOLDER}/file-${String(i).padStart(3, "0")}.js`),
  );
  await page.setViewportSize({ width: 1440, height: 900 });
  await page.goto("/?mode=repos&representation=table");
  const explorer = page.getByTestId("explorer-panel");
  await explorer.getByRole("tab", { name: "Files", exact: true }).click();
  const tree = explorer.getByRole("tree", { name: "Files", exact: true });
  const folder = tree.getByTitle(FOLDER_TITLE, { exact: true });
  await expect(folder).toBeVisible();
  return { explorer, tree, folder };
}

async function assertNormalScrollEnd(page: Page, tree: Locator, info: TestInfo, name: string) {
  // Mouse coordinates avoid locator actions that scroll a focused/target row
  // into view through overflow-hidden ancestors and conceal the original bug.
  const viewport = await tree.evaluate((element) => {
    const rect = element.getBoundingClientRect();
    const panel = document.getElementById("explorer-tabpanel")!.getBoundingClientRect();
    return { x: rect.left + rect.width / 2, y: (Math.max(rect.top, panel.top) + Math.min(rect.bottom, panel.bottom)) / 2,
      scrollHeight: element.scrollHeight, clientHeight: element.clientHeight };
  });
  expect(viewport.scrollHeight, "this is a real overflowing tree").toBeGreaterThan(viewport.clientHeight);
  await page.mouse.move(viewport.x, viewport.y);
  await page.mouse.wheel(0, viewport.scrollHeight * 2);
  await expect.poll(() => tree.evaluate((el) => Math.abs(el.scrollHeight - el.clientHeight - el.scrollTop))).toBeLessThanOrEqual(1);
  await renderedFrame(page);
  const geometry = await tree.evaluate((element) => {
    const rect = (el: Element) => {
      const r = el.getBoundingClientRect();
      return { top: r.top, bottom: r.bottom, left: r.left, right: r.right, height: r.height };
    };
    const panel = document.getElementById("explorer-tabpanel")!;
    const rows = element.querySelectorAll('[role="treeitem"]');
    // All-files/reset ordering can end in another folder's or top-level file.
    const last = rows[rows.length - 1];
    const rowRect = rect(last);
    const treeRect = rect(element);
    const panelRect = rect(panel);
    const ancestors = [];
    for (let parent = element.parentElement; parent; parent = parent.parentElement) {
      ancestors.push({ tag: parent.tagName, id: parent.id, scrollTop: parent.scrollTop });
    }
    const hit = document.elementFromPoint((rowRect.left + rowRect.right) / 2, (rowRect.top + rowRect.bottom) / 2);
    return { tree: treeRect, panel: panelRect, last: rowRect, lastTitle: last.getAttribute("title"),
      visibleTop: Math.max(treeRect.top + element.clientTop, panelRect.top),
      visibleBottom: Math.min(treeRect.top + element.clientTop + element.clientHeight, panelRect.bottom),
      scrollTop: element.scrollTop, scrollHeight: element.scrollHeight, clientHeight: element.clientHeight,
      hitLastRow: hit != null && last.contains(hit), ancestors };
  });
  const geometryPath = info.outputPath(`${name}-geometry.json`);
  await writeFile(geometryPath, JSON.stringify(geometry, null, 2));
  await info.attach(`${name}-geometry.json`, { path: geometryPath, contentType: "application/json" });
  const screenshot = info.outputPath(`${name}.png`);
  await page.screenshot({ path: screenshot });
  await info.attach(`${name}.png`, { path: screenshot, contentType: "image/png" });
  expect(geometry.lastTitle, "the measured final rendered row has canonical file identity").toMatch(/\.js$/);
  expect(geometry.ancestors.every((ancestor) => ancestor.scrollTop === 0), "clipping ancestors must not have been scrolled to mask clipping").toBe(true);
  expect(geometry.last.top).toBeGreaterThanOrEqual(geometry.visibleTop - 1);
  expect(geometry.last.bottom, "the final row is wholly visible at maximum ordinary scroll").toBeLessThanOrEqual(geometry.visibleBottom + 1);
  expect(geometry.hitLastRow, "the final row receives pointer hit testing").toBe(true);
}

async function assertSelectedFolder(page: Page, tree: Locator) {
  const header = page.getByLabel("Selected folder", { exact: true });
  await expect(header).toContainText(`Files in ${FOLDER}`);
  const label = header.locator("p");
  const metrics = await label.evaluate((el) => ({ height: el.getBoundingClientRect().height, lineHeight: Number.parseFloat(getComputedStyle(el).lineHeight) }));
  expect(metrics.height, "the real long folder header wraps").toBeGreaterThan(metrics.lineHeight + 1);
  await expect(tree.getByTitle("ui-folder-other/entry.js", { exact: true })).toHaveCount(0);
  await expect(tree.locator('[role="treeitem"][aria-level="1"]')).toHaveCount(1);
  await expect(tree.locator('[role="treeitem"][tabindex="0"]')).toHaveCount(1);
  return header;
}

test("@files-overflow real folder rows remain visible through selection, reset and narrow layout", async ({ page, request }, info) => {
  const { tree, folder } = await openOverflowFiles(page, request);
  await folder.focus();
  await folder.press("ArrowRight");
  await expect(tree.getByTitle(`${FOLDER}/file-063.js`, { exact: true })).toHaveCount(1);
  await assertNormalScrollEnd(page, tree, info, "all-files-before-selection");
  await folder.press("Enter");
  const header = await assertSelectedFolder(page, tree);
  await assertNormalScrollEnd(page, tree, info, "selected-folder-1440");
  await header.getByRole("button", { name: "Show all files", exact: true }).click();
  await expect(header).toHaveCount(0);
  await expect(folder).toBeFocused();
  await assertNormalScrollEnd(page, tree, info, "all-files-after-reset");
  await folder.press("Enter");
  await page.setViewportSize({ width: 1200, height: 900 });
  await assertSelectedFolder(page, tree);
  await assertNormalScrollEnd(page, tree, info, "selected-folder-1200");
});

test("@files-overflow retained refresh error reserves space above real folder rows", async ({ page, request }, info) => {
  await installDeliveryReceipts(page);
  const events = { pending: "" };
  await page.route("**/api/v1/events", (route) => {
    const body = `retry: 200\n${events.pending}\n`;
    events.pending = "";
    return route.fulfill({ status: 200, contentType: "text/event-stream", body });
  });
  const { explorer, tree, folder } = await openOverflowFiles(page, request);
  await folder.press("Enter");
  await assertSelectedFolder(page, tree);
  const message = "Files overflow regression: the repository refresh is temporarily unavailable; the previously loaded real files remain available below this wrapped error.";
  const path = /\/api\/v1\/repos(?:\?|$)/;
  await page.route(path, (route) => route.fulfill({ status: 503, json: { error: "fixture_refresh_unavailable", message } }));
  try {
    const beforeEvent = await eventCount(page, "graph:updated");
    const beforeReply = await decodedReplyCount(page, "/api/v1/repos");
    events.pending = "event: graph:updated\ndata: {}\n\n";
    await expect.poll(() => eventCount(page, "graph:updated")).toBeGreaterThan(beforeEvent);
    await waitForDecodedReply(page, "/api/v1/repos", {}, beforeReply);
    const alert = explorer.getByRole("alert");
    await expect(alert).toHaveText(message);
    const metrics = await alert.evaluate((el) => ({ height: el.getBoundingClientRect().height, lineHeight: Number.parseFloat(getComputedStyle(el).lineHeight) }));
    expect(metrics.height).toBeGreaterThan(metrics.lineHeight * 2);
    await assertSelectedFolder(page, tree);
    await expect(tree.getByTitle(`${FOLDER}/file-063.js`, { exact: true })).toHaveCount(1);
    await assertNormalScrollEnd(page, tree, info, "selected-folder-retained-alert");
  } finally {
    await page.unroute(path);
  }
});
