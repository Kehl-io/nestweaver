import { expect, test, type APIRequestContext, type Page } from "@playwright/test";

interface SymbolCandidate {
  uid: string;
  name: string;
  kind: string;
}

async function findSymbol(request: APIRequestContext, query: string): Promise<SymbolCandidate> {
  for (let attempt = 0; attempt < 40; attempt += 1) {
    const response = await request.get(`/api/v1/search?q=${encodeURIComponent(query)}&limit=8`);
    if (response.ok()) {
      const symbols = (await response.json()) as SymbolCandidate[];
      const match = symbols.find((s) => s.name === query) ?? symbols[0];
      if (match) return match;
    }
    await new Promise((resolve) => setTimeout(resolve, 250));
  }
  throw new Error(`fixture has no searchable symbol for ${query}`);
}

function persistRepresentation(page: Page, representationMode: string) {
  return page.addInitScript((mode) => {
    window.localStorage.setItem(
      "nestweaver-ui",
      JSON.stringify({
        state: { layoutMode: "panels", representationMode: mode, viewMode: "graph" },
        version: 6,
      }),
    );
  }, representationMode);
}

function urlParam(page: Page, name: string) {
  return new URL(page.url()).searchParams.get(name);
}

test.describe("Deep links (batch 11)", () => {
  test("a deep link without representation opens the graph, not the persisted JSON mode (nw-571)", async ({
    page,
    request,
  }) => {
    const symbol = await findSymbol(request, "greet");
    await page.setViewportSize({ width: 1440, height: 900 });
    await persistRepresentation(page, "json");
    await page.goto(`/?mode=context&node=${encodeURIComponent(symbol.uid)}&kind=${symbol.kind}`);
    await expect(page.getByTestId("graph-panel")).toBeVisible({ timeout: 15_000 });

    await expect(page.getByRole("tab", { name: "Graph representation" })).toHaveAttribute(
      "aria-selected",
      "true",
    );
    await expect(page.getByRole("region", { name: "JSON result", exact: true })).toHaveCount(0);
    // Let the URL sync settle, then check nothing wrote the stale mode back.
    await expect.poll(() => urlParam(page, "node")).toBe(symbol.uid);
    expect(urlParam(page, "representation")).toBeNull();
  });

  test("counterweight: an explicit representation=table deep link still restores table (nw-571)", async ({
    page,
    request,
  }) => {
    const symbol = await findSymbol(request, "greet");
    await page.setViewportSize({ width: 1440, height: 900 });
    await persistRepresentation(page, "graph");
    await page.goto(
      `/?mode=context&node=${encodeURIComponent(symbol.uid)}&kind=${symbol.kind}&representation=table`,
    );
    await expect(page.getByTestId("graph-panel")).toBeVisible({ timeout: 15_000 });
    await expect(page.getByRole("tab", { name: "Table representation" })).toHaveAttribute(
      "aria-selected",
      "true",
    );
    await expect.poll(() => urlParam(page, "representation")).toBe("table");
  });

  test("?kind=repo survives the URL and filters Overview to repos (nw-595)", async ({ page, request }) => {
    const overview = await request.get("/api/v1/overview?limit=96");
    expect(overview.ok()).toBeTruthy();
    const body = (await overview.json()) as { landmarks: { kind: string }[] };
    const repoCount = body.landmarks.filter((l) => l.kind === "repo").length;
    expect(repoCount, "fixture overview has a repo landmark").toBeGreaterThan(0);
    expect(
      body.landmarks.length,
      "fixture overview has non-repo landmarks to filter out",
    ).toBeGreaterThan(repoCount);

    await page.setViewportSize({ width: 1440, height: 900 });
    await persistRepresentation(page, "graph");
    await page.goto("/?kind=repo&representation=table");
    await expect(page.getByTestId("graph-panel")).toBeVisible({ timeout: 15_000 });

    const filter = page.getByRole("status", { name: "Overview filter" });
    await expect(filter).toContainText(/repos/i);
    const rows = page.getByRole("table").getByRole("row");
    // header row + one row per repo landmark
    await expect(rows).toHaveCount(repoCount + 1);
    await expect.poll(() => urlParam(page, "kind")).toBe("repo");
    expect(new URL(page.url()).pathname + new URL(page.url()).search).not.toBe("/");

    // Clearing the filter restores the full overview and drops the param.
    await filter.getByRole("button", { name: "Show all kinds" }).click();
    await expect.poll(() => rows.count()).toBeGreaterThan(repoCount + 1);
    await expect.poll(() => urlParam(page, "kind")).toBeNull();
  });

  test("counterweight: ?mode= still switches tabs and a node deep link keeps its kind (nw-595)", async ({
    page,
    request,
  }) => {
    const symbol = await findSymbol(request, "greet");
    await page.setViewportSize({ width: 1440, height: 900 });
    await persistRepresentation(page, "graph");
    await page.goto("/?mode=repos");
    await expect(
      page.getByRole("group", { name: "Graph mode" }).getByRole("button", { name: /repos/i }),
    ).toHaveAttribute("aria-pressed", "true", { timeout: 15_000 });
    await expect(page.getByRole("status", { name: "Overview filter" })).toHaveCount(0);

    await page.goto(`/?node=${encodeURIComponent(symbol.uid)}&kind=${symbol.kind}`);
    await expect(page.getByTestId("graph-panel")).toBeVisible({ timeout: 15_000 });
    await expect.poll(() => urlParam(page, "kind")).toBe(symbol.kind);
    await expect(page.getByRole("status", { name: "Overview filter" })).toHaveCount(0);
  });
});
