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
});
