import { expect, test, type Locator, type Page } from "@playwright/test";

// nw-658: the top scene breadcrumb (`SceneBreadcrumbs.tsx`) rendered
// overlapping segments that read as "All Dcontext" on 10.1.2 — root-caused to
// the home crumb button having `min-w-0` (to let it compress on narrow
// windows) with no `overflow-hidden` of its own, so once the flex row shrank
// the button below its label span's `min-w-[3rem]`, the span kept painting
// at full width and bled over the chevron and the next crumb.
//
// The first fix (`overflow-hidden` on the button) stopped the bleed but
// introduced a second bug caught in review: with nothing to protect the
// button's *own* size, the outer nav's flex-shrink kept squeezing it below
// its icon's width, clipping the house icon in half and hiding the label
// entirely — non-overlap satisfied by making the crumb unreadable, not by
// making the row fit.
//
// Root cause of *that*: the nav's real budget at the default 1280px viewport
// is far smaller than the header suggests — `WorkspaceToolbar`'s right-hand
// icon cluster (undo/redo/center/layout/minimap/representation tabs) is
// `shrink-0` and eats a fixed chunk of the header first, leaving the
// breadcrumb `<nav>` only ~190px (measured live). Within that budget, the
// lens crumb, both chevrons, and the (redundant — `RepresentationTabs`
// already shows this) trailing representation crumb were *all*
// `shrink-0`/fixed, while only the home button (via `min-w-0`, no floor) and
// the node/selection crumb (via a bare `min-w-[4rem]`, no `shrink-0`) could
// give ground — so the two crumbs that most need to stay legible (workspace
// context, then selection) were the only ones asked to absorb the deficit,
// and the flex algorithm doesn't know the home crumb matters more.
//
// Fix: give the home button `shrink-0` instead of `min-w-0`. That makes its
// rendered size equal to its content's natural size (icon + label, capped by
// the label's own `max-w-[8rem]`) — it is *never* shrunk below what it needs,
// so the icon can't be clipped and the label always gets its full truncated
// allowance. The node/selection crumb is left as the row's one flexible
// (non-`shrink-0`) segment, so it — and, if the row is still short on room,
// whatever comes after it — is what gives way first, exactly matching "later
// crumbs truncate first". `overflow-hidden` stays on the button as a
// containment backstop even though `shrink-0` means it should no longer be
// load-bearing on its own.

function crumbNav(page: Page): Locator {
  return page.getByRole("navigation", { name: "Scene breadcrumbs" });
}

// Each crumb's *visible, painted* extent — its own `getBoundingClientRect()`
// intersected with every ancestor's rect up to the nav, but only for
// ancestors that actually clip (`overflow` other than `visible`). A raw
// `getBoundingClientRect()` isn't enough on its own: it reports the
// element's laid-out box, which can genuinely extend past a shrunken
// ancestor's box without anything actually being painted there (that's what
// let the original bleed go undetected), or conversely stay at its full
// laid-out size even though a clipping ancestor is hiding all of it (which
// would make a naive check think a fully-clipped crumb is still there).
async function visibleRects(nav: Locator, selector: string) {
  const rects = await nav.evaluate((navEl, sel) => {
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
    return Array.from(navEl.querySelectorAll(sel)).map(visibleRect);
  }, selector);
  return rects.filter((r) => r.right - r.x > 0 && r.bottom - r.y > 0);
}

// `.truncate` is the deepest, most-specific text-bearing element for each
// crumb (the home crumb's label is a *nested* span inside its button, while
// the lens/node crumbs carry the class directly).
function segmentRects(nav: Locator) {
  return visibleRects(nav, ".truncate");
}

function rectsOverlap(
  a: { x: number; y: number; right: number; bottom: number },
  b: { x: number; y: number; right: number; bottom: number },
): boolean {
  return a.x < b.right && b.x < a.right && a.y < b.bottom && b.y < a.bottom;
}

// Non-overlap alone is satisfiable by squeezing the home crumb down to
// nothing (that's the regression this guards against): assert its visible,
// clip-intersected width is at least its icon's width plus a legible sliver
// of label, so "does not overlap" can't be met by "is not there".
async function assertHomeCrumbLegible(nav: Locator) {
  const [iconRect] = await visibleRects(nav, "button:first-of-type svg");
  const [buttonRect] = await visibleRects(nav, "button:first-of-type");
  expect(iconRect, "home crumb icon should be visible").toBeTruthy();
  expect(buttonRect, "home crumb button should be visible").toBeTruthy();

  const iconWidth = iconRect.right - iconRect.x;
  const buttonWidth = buttonRect.right - buttonRect.x;

  // The icon itself must be fully visible (not clipped in half), and the
  // button must have room for the icon plus at least ~40px of label —
  // enough for a couple of truncated characters and an ellipsis, not just a
  // sliver.
  expect(iconWidth, "home crumb icon must not be clipped").toBeGreaterThanOrEqual(13);
  expect(
    buttonWidth,
    `home crumb visible width (${buttonWidth}px) should cover its icon (${iconWidth}px) plus a legible label`,
  ).toBeGreaterThanOrEqual(iconWidth + 40);
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
    await assertHomeCrumbLegible(nav);
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
    await assertHomeCrumbLegible(nav);

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
