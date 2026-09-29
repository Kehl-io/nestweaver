import { expect, test, type Page } from "@playwright/test";

// nw-565: Tab walked the whole Files tree (~192 stops on a real index)
// before reaching the graph chrome, and there was no skip link.

async function openPanels(page: Page) {
  await page.setViewportSize({ width: 1440, height: 900 });
  await page.addInitScript(() => {
    window.localStorage.setItem(
      "nestweaver-ui",
      JSON.stringify({
        state: {
          layoutMode: "panels",
          representationMode: "graph",
          viewMode: "graph",
          explorerTab: "files",
        },
        version: 6,
      }),
    );
  });
  await page.goto("/");
  await expect(page.getByTestId("graph-panel")).toBeVisible({ timeout: 15_000 });
  await expect(page.getByRole("tree", { name: "Files" })).toBeVisible();
}

function focusedIsInside(page: Page, testId: string) {
  return page.evaluate(
    (id) => Boolean(document.activeElement?.closest(`[data-testid="${id}"]`)),
    testId,
  );
}

test.describe("Keyboard navigation (nw-565)", () => {
  test("the first Tab stop is a skip link that lands on the graph chrome", async ({ page }) => {
    await openPanels(page);
    await page.locator("body").focus();
    await page.keyboard.press("Tab");
    const skip = page.getByRole("link", { name: "Skip to graph" });
    await expect(skip).toBeFocused();
    await expect(skip).toBeVisible();

    await page.keyboard.press("Enter");
    await expect(page.getByRole("main")).toBeFocused();
    await page.keyboard.press("Tab");
    expect(await focusedIsInside(page, "graph-panel")).toBe(true);
  });

  test("the Files tree is a single Tab stop", async ({ page }) => {
    await openPanels(page);
    const tree = page.getByRole("tree", { name: "Files" });
    const items = tree.getByRole("treeitem");
    expect(await items.count()).toBeGreaterThan(1);
    // Exactly one item is in the Tab sequence; nothing else inside is.
    await expect(tree.locator('[tabindex="0"]')).toHaveCount(1);
    await expect(tree.locator("button, a[href], input")).toHaveCount(0);

    await page.getByRole("tab", { name: "Files", exact: true }).focus();
    await page.keyboard.press("Tab");
    await expect(items.first()).toBeFocused();
    await page.keyboard.press("Tab");
    expect(await focusedIsInside(page, "explorer-panel")).toBe(false);
  });

  test("counterweight: arrow keys walk, expand, collapse and open files", async ({ page }) => {
    await openPanels(page);
    const tree = page.getByRole("tree", { name: "Files" });
    const repo = tree.getByRole("treeitem").first();
    await repo.focus();
    await expect(repo).toHaveAttribute("aria-expanded", "true");

    // Left collapses the repo; Right expands it again.
    await page.keyboard.press("ArrowLeft");
    await expect(repo).toHaveAttribute("aria-expanded", "false");
    await expect(tree.getByRole("treeitem")).toHaveCount(1);
    await page.keyboard.press("ArrowRight");
    await expect(repo).toHaveAttribute("aria-expanded", "true");

    // Right on an open node moves to its first child; Down walks on.
    await page.keyboard.press("ArrowRight");
    const second = tree.getByRole("treeitem").nth(1);
    await expect(second).toBeFocused();
    await expect(second).toHaveAttribute("tabindex", "0");
    await expect(repo).toHaveAttribute("tabindex", "-1");

    // End reaches the last visible item; Enter on a file selects it.
    await page.keyboard.press("End");
    const last = tree.getByRole("treeitem").last();
    await expect(last).toBeFocused();
    const lastExpandable = await last.getAttribute("aria-expanded");
    expect(lastExpandable, "the last visible item is a file").toBeNull();
    await page.keyboard.press("Enter");
    await expect.poll(() => new URL(page.url()).searchParams.get("kind")).toBe("file");
    await expect(last).toHaveAttribute("aria-selected", "true");

    // Home returns to the first item.
    await page.keyboard.press("Home");
    await expect(repo).toBeFocused();
  });

  test("tree type-ahead leaves global shortcuts working (c, m, t, /)", async ({ page }) => {
    await openPanels(page);
    const tree = page.getByRole("tree", { name: "Files" });
    const items = tree.getByRole("treeitem");
    await items.first().focus();

    const minimap = page.getByRole("button", { name: /minimap$/ });
    const before = await minimap.getAttribute("aria-pressed");
    await page.keyboard.press("m");
    await expect(minimap).not.toHaveAttribute("aria-pressed", before ?? "");

    // A letter with no app binding is the tree's type-ahead.
    const reserved = new Set(["1", "2", "3", "4", "5", "6", "c", "m", "t", "i", "p"]);
    const firsts = (await items.allTextContents()).map((label) => label.replace(/^[^A-Za-z0-9]+/, "").charAt(0).toLowerCase());
    // The first item (after the focused one) whose letter no earlier item shares.
    const target = firsts.findIndex(
      (ch, i) => i > 0 && /[a-z0-9]/.test(ch) && !reserved.has(ch) && !firsts.slice(1, i).includes(ch),
    );
    expect(target, `fixture tree has a type-ahead target in ${firsts.join("")}`).toBeGreaterThan(0);
    const letter = firsts[target];
    await items.first().focus();
    await page.keyboard.press(letter);
    await expect(items.nth(target)).toBeFocused();

    // Modified letters are not type-ahead.
    await items.first().focus();
    await page.keyboard.press(`Alt+${letter}`);
    await expect(items.first()).toBeFocused();
    await items.nth(target).focus();
    await expect(items.nth(target)).toBeFocused();

    // "/" still jumps to search.
    await page.keyboard.press("/");
    await expect(page.getByTestId("search-input")).toBeFocused();
  });
});
