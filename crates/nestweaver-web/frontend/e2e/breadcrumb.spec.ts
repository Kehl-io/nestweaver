import { expect, test, type Locator, type Page } from "@playwright/test";

// nw-658: the top scene breadcrumb (`SceneBreadcrumbs.tsx`) rendered
// overlapping segments that read as "All Dcontext" on 10.1.2. Root cause: the
// home crumb button had `min-w-0` (to let it compress on narrow windows) but
// no `overflow-hidden` of its own, so once the flex row shrank the button
// below its label span's `min-w-[3rem]`, the span's text — already clipped
// correctly *within its own box* — bled out past the button's now-smaller
// box and painted over the chevron and the next crumb instead of being
// contained. `overflow-hidden` on the button fixes containment without
// touching the intentional shrink behavior.

function crumbNav(page: Page): Locator {
  return page.getByRole("navigation", { name: "Scene breadcrumbs" });
}

async function segmentRects(nav: Locator) {
  // `.truncate` is the deepest, most-specific text-bearing element for each
  // crumb (the home crumb's label is a *nested* span inside its button,
  // while the lens/node crumbs carry the class directly). A raw
  // `getBoundingClientRect()` on that element isn't enough on its own: it
  // reports the element's *laid-out* box, which can genuinely extend past a
  // shrunken ancestor's box without anything actually being painted there,
  // as long as some ancestor between it and the nav clips overflow. So what
  // we actually want is each crumb's *visible, painted* extent — its own
  // rect intersected with every ancestor's rect up to the nav, but only for
  // ancestors that actually clip (`overflow` other than `visible`). That is
  // precisely what nw-658 was missing on the home crumb's button: nothing
  // between the oversized label span and the nav clipped it, so the label
  // painted straight over the next crumb.
  const rects = await nav.evaluate((navEl) => {
    function visibleRect(el: Element) {
      let rect = el.getBoundingClientRect();
      let node: Element | null = el.parentElement;
      while (node && node !== navEl.parentElement) {
        const style = getComputedStyle(node);
        if (style.overflow !== "visible") {
          const clip = node.getBoundingClientRect();
          const left = Math.max(rect.left, clip.left);
          const top = Math.max(rect.top, clip.top);
          const right = Math.min(rect.right, clip.right);
          const bottom = Math.min(rect.bottom, clip.bottom);
          rect = new DOMRect(left, top, Math.max(0, right - left), Math.max(0, bottom - top));
        }
        if (node === navEl) break;
        node = node.parentElement;
      }
      return { x: rect.x, y: rect.y, right: rect.right, bottom: rect.bottom };
    }
    return Array.from(navEl.querySelectorAll(".truncate")).map(visibleRect);
  });
  return rects.filter((r) => r.right - r.x > 0 && r.bottom - r.y > 0);
}

function rectsOverlap(
  a: { x: number; y: number; right: number; bottom: number },
  b: { x: number; y: number; right: number; bottom: number },
): boolean {
  return a.x < b.right && b.x < a.right && a.y < b.bottom && b.y < a.bottom;
}

async function assertNoOverlap(page: Page, nav: Locator) {
  // Wait for webfonts to finish swapping in: the crumb labels render with a
  // fallback font first, and nw-658 only shows once the real (wider)
  // webfont glyphs are in and the flex row actually has to compress —
  // checking immediately after mount would silently pass on the narrower
  // fallback-font layout and miss the bug.
  await page.evaluate(() => document.fonts.ready);
  const rects = await segmentRects(nav);
  expect(rects.length).toBeGreaterThan(1);
  for (let i = 0; i < rects.length; i += 1) {
    for (let j = i + 1; j < rects.length; j += 1) {
      expect(
        rectsOverlap(rects[i], rects[j]),
        `crumb ${i} ${JSON.stringify(rects[i])} overlaps crumb ${j} ${JSON.stringify(rects[j])}`,
      ).toBe(false);
    }
  }
}

test.describe("Scene breadcrumbs (nw-658)", () => {
  test("segments do not overlap at the default window width", async ({ page }) => {
    // Intentionally do not call setViewportSize: the bug reproduces at
    // Playwright's default viewport, which is the "default window width"
    // the backlog item's DONE WHEN refers to.
    await page.goto("/");
    await expect(page.getByTestId("graph-panel")).toBeVisible({ timeout: 15_000 });

    const nav = crumbNav(page);
    await expect(nav).toBeVisible();
    await assertNoOverlap(page, nav);
  });

  test("segments do not overlap with a long workspace label at a narrow width", async ({
    page,
    request,
  }) => {
    const response = await request.get("/api/v1/workspaces");
    expect(response.ok()).toBeTruthy();
    const catalog = (await response.json()) as {
      workspaces: { id: string; label: string }[];
    };
    const longest = catalog.workspaces.reduce((best, current) =>
      current.label.length > best.label.length ? current : best,
    );

    // Narrower than default: squeezes every crumb toward its min-width at
    // once, the same failure mode nw-658 hit, just harder to miss.
    await page.setViewportSize({ width: 640, height: 720 });
    await page.goto("/");
    await expect(page.getByTestId("graph-panel")).toBeVisible({ timeout: 15_000 });

    await page.getByLabel("Workspace").click();
    await page.getByRole("option", { name: new RegExp(longest.label.slice(0, 8)) }).click();

    const nav = crumbNav(page);
    await expect(nav).toBeVisible();
    await assertNoOverlap(page, nav);

    // Long labels must still communicate their full text via a tooltip
    // rather than being silently cut off.
    const homeCrumb = nav.getByRole("button").first();
    await expect(homeCrumb).toHaveAttribute("title", new RegExp(longest.label));
  });

  test("counterweight: clicking the root crumb still navigates to overview", async ({ page }) => {
    await page.goto("/");
    await expect(page.getByTestId("graph-panel")).toBeVisible({ timeout: 15_000 });

    const nav = crumbNav(page);
    const lensCrumb = nav.getByTitle(/^Lens:/);
    await expect(lensCrumb).toHaveText("Overview");

    const searchInput = page.getByTestId("search-input");
    await searchInput.fill("greet");
    const firstResult = page.getByRole("listbox").getByRole("option").first();
    await firstResult.waitFor({ timeout: 10_000 });
    await firstResult.click();

    // Selecting a result navigates the scene (exploreNode → graphMode
    // "context"), so the lens crumb moves off "Overview" — that's the
    // observable proof the crumb-driven navigation still works end to end.
    await expect(lensCrumb).not.toHaveText("Overview");

    // Structural, not title-text-based: the home crumb is always the first
    // button in the breadcrumb nav, and this assertion should hold
    // regardless of the exact tooltip wording.
    await nav.locator("button").first().click();
    await expect(lensCrumb).toHaveText("Overview");
  });
});
