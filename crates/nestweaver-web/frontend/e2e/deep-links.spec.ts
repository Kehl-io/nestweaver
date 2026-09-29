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

    const overviewKinds: (string | null)[] = [];
    page.on("request", (req) => {
      const url = new URL(req.url());
      if (url.pathname === "/api/v1/overview") overviewKinds.push(url.searchParams.get("kind"));
    });
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
    // The server filters before its per-kind caps, so the client asks for it.
    expect(overviewKinds).toContain("repo");
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

  test("an invalid ?node= deep link fetches once, shows one not-found state, and disables actions (nw-567)", async ({
    page,
  }) => {
    const missing = "sym:does-not-exist";
    let symbolFetches = 0;
    page.on("request", (req) => {
      if (new URL(req.url()).pathname.startsWith("/api/v1/symbol/")) symbolFetches += 1;
    });
    await page.setViewportSize({ width: 1440, height: 900 });
    await persistRepresentation(page, "graph");
    await page.goto(`/?node=${encodeURIComponent(missing)}&kind=Function`);
    await expect(page.getByTestId("graph-panel")).toBeVisible({ timeout: 15_000 });

    const details = page.getByTestId("detail-panel");
    const evidence = page.getByRole("complementary", { name: "Source and note evidence" });
    await expect(details.getByRole("heading", { name: "Node not found" })).toBeVisible();
    await expect(evidence.getByRole("heading", { name: "Node not found" })).toBeVisible();
    await expect(details).not.toContainText("symbol '");
    await expect(
      page.getByRole("navigation", { name: "Scene breadcrumbs" }).getByTitle(/not found/i),
    ).toBeVisible();

    const actions = details.getByRole("group", { name: "Node actions" }).getByRole("button");
    await expect(actions.first()).toBeVisible();
    for (const action of await actions.all()) {
      await expect(action).toHaveAttribute("aria-disabled", "true");
    }

    // Details, Evidence and the breadcrumb have all settled on the miss
    // above, so every consumer has asked. A follow-up render (a round trip
    // through another representation) must not ask again.
    expect(symbolFetches).toBe(1);
    const representation = page.getByRole("tablist", { name: "Result representation" });
    await representation.getByRole("tab", { name: "Table representation" }).click();
    await expect(page.getByRole("table")).toBeVisible();
    await representation.getByRole("tab", { name: "Graph representation" }).click();
    await expect(details.getByRole("heading", { name: "Node not found" })).toBeVisible();
    await expect(evidence.getByRole("heading", { name: "Node not found" })).toBeVisible();
    expect(symbolFetches).toBe(1);

    // The not-found state offers a way out.
    await details.getByRole("button", { name: "Clear selection" }).click();
    await expect(details.getByRole("heading", { name: "Ready when you select a node" })).toBeVisible();
    await expect.poll(() => urlParam(page, "node")).toBeNull();
  });

  test("a missing node that appears after a graph update recovers without reselecting (nw-567)", async ({
    page,
    request,
  }) => {
    const symbol = await findSymbol(request, "greet");
    // Stand in for a re-index: the symbol 404s until the "index" lands, and
    // the SSE stream then delivers one graph:updated event.
    let indexed = false;
    let pendingUpdate = false;
    await page.route("**/api/v1/symbol/**", (route) =>
      indexed
        ? route.continue()
        : route.fulfill({
            status: 404,
            contentType: "application/json",
            body: JSON.stringify({ error: "not_found", message: "symbol not found" }),
          }),
    );
    await page.route("**/api/v1/events", (route) => {
      const body = pendingUpdate ? "retry: 200\nevent: graph:updated\ndata: {}\n\n" : "retry: 200\n\n";
      pendingUpdate = false;
      return route.fulfill({ status: 200, contentType: "text/event-stream", body });
    });
    await page.setViewportSize({ width: 1440, height: 900 });
    await persistRepresentation(page, "graph");
    await page.goto(`/?node=${encodeURIComponent(symbol.uid)}&kind=${symbol.kind}`);

    const details = page.getByTestId("detail-panel");
    const evidence = page.getByRole("complementary", { name: "Source and note evidence" });
    const open = details.getByRole("group", { name: "Node actions" }).first().getByRole("button", { name: /^Open source/ });
    await expect(details.getByRole("heading", { name: "Node not found" })).toBeVisible({ timeout: 15_000 });
    await expect(evidence.getByRole("heading", { name: "Node not found" })).toBeVisible();
    await expect(open).toHaveAttribute("aria-disabled", "true");

    indexed = true;
    pendingUpdate = true;
    await expect(details.getByRole("heading", { name: "Node not found" })).toHaveCount(0, { timeout: 10_000 });
    await expect(evidence.getByRole("heading", { name: "Node not found" })).toHaveCount(0);
    await expect(evidence.getByRole("heading", { name: symbol.name })).toBeVisible();
    await expect(open).toHaveAttribute("aria-disabled", "false");
    await expect(page.getByRole("navigation", { name: "Scene breadcrumbs" })).toContainText(symbol.name);
    expect(urlParam(page, "node")).toBe(symbol.uid);
  });

  test("counterweight: a real Function deep link still enables the actions (nw-567)", async ({
    page,
    request,
  }) => {
    const symbol = await findSymbol(request, "greet");
    await page.setViewportSize({ width: 1440, height: 900 });
    await persistRepresentation(page, "graph");
    await page.goto(`/?node=${encodeURIComponent(symbol.uid)}&kind=${symbol.kind}`);
    const details = page.getByTestId("detail-panel");
    await expect(details.getByText(symbol.name).first()).toBeVisible({ timeout: 15_000 });
    const open = details
      .getByRole("group", { name: "Node actions" })
      .first()
      .getByRole("button", { name: /^Open source/ });
    await expect(open).toHaveAttribute("aria-disabled", "false");
    await expect(details.getByRole("heading", { name: "Node not found" })).toHaveCount(0);
  });

  test.describe("Path dialog target (nw-568)", () => {
    async function openPathDialog(page: Page, request: APIRequestContext) {
      const from = await findSymbol(request, "releaseA");
      await page.setViewportSize({ width: 1440, height: 900 });
      await persistRepresentation(page, "graph");
      await page.goto(`/?node=${encodeURIComponent(from.uid)}&kind=${from.kind}`);
      const details = page.getByTestId("detail-panel");
      await expect(details.getByText("releaseA").first()).toBeVisible({ timeout: 15_000 });
      await details
        .getByRole("group", { name: "Node actions" })
        .first()
        .getByRole("button", { name: /^Path$/ })
        .click();
      const dialog = page.getByRole("dialog", { name: "Find path" });
      await expect(dialog).toBeVisible();
      return { dialog, details, from };
    }

    function pathRequests(page: Page) {
      const seen: string[] = [];
      page.on("request", (req) => {
        const url = new URL(req.url());
        if (url.pathname.startsWith("/api/v1/paths/")) {
          seen.push(decodeURIComponent(url.pathname.slice("/api/v1/paths/".length)));
        }
      });
      return seen;
    }

    test("a unique name resolves to its uid and finds the path", async ({ page, request }) => {
      const target = await findSymbol(request, "releaseC");
      const requests = pathRequests(page);
      const { dialog, from } = await openPathDialog(page, request);
      // The From label is the node's name, not its raw uid.
      await expect(dialog).toContainText("releaseA");
      await expect(dialog).not.toContainText(from.uid);

      await dialog.getByRole("textbox", { name: "Path target" }).fill("releaseC");
      await dialog.getByRole("textbox", { name: "Path target" }).press("Enter");
      await expect(page.getByRole("button", { name: /^Path 1:/ }).first()).toBeVisible({ timeout: 10_000 });
      expect(requests).toEqual([`${from.uid}/${target.uid}`]);
    });

    test("an ambiguous name offers a choice before querying", async ({ page, request }) => {
      const search = await request.get("/api/v1/search?q=speak&limit=20");
      const speaks = ((await search.json()) as SymbolCandidate[]).filter((s) => s.name === "speak");
      expect(speaks.length, "fixture has two methods named speak").toBeGreaterThan(1);
      const requests = pathRequests(page);
      const { dialog, from } = await openPathDialog(page, request);

      await dialog.getByRole("textbox", { name: "Path target" }).fill("speak");
      await dialog.getByRole("button", { name: "Find" }).click();
      const choices = dialog.getByRole("listbox", { name: "Matching nodes" });
      await expect(choices.getByRole("option")).toHaveCount(speaks.length);
      expect(requests).toEqual([]);

      await choices.getByRole("option").nth(1).click();
      await expect.poll(() => requests.length).toBe(1);
      expect(speaks.map((s) => `${from.uid}/${s.uid}`)).toContain(requests[0]);
    });

    test("an unknown name says so instead of querying a literal segment", async ({ page, request }) => {
      const requests = pathRequests(page);
      const { dialog } = await openPathDialog(page, request);
      await dialog.getByRole("textbox", { name: "Path target" }).fill("zzzNoSuchNodeName");
      await dialog.getByRole("button", { name: "Find" }).click();
      await expect(dialog.getByRole("alert")).toContainText(/No node named/);
      expect(requests).toEqual([]);
    });

    test("name resolution is scoped to the active workspace", async ({ page, request }) => {
      const catalog = (await (await request.get("/api/v1/workspaces")).json()) as {
        workspaces: { id: string; type: string }[];
      };
      const workspace = catalog.workspaces.find((w) => w.type === "repo");
      expect(workspace, "fixture has a repo workspace").toBeTruthy();
      const from = await findSymbol(request, "releaseA");
      const target = await findSymbol(request, "releaseC");
      const lookups: URL[] = [];
      page.on("request", (req) => {
        const url = new URL(req.url());
        if (url.pathname === "/api/v1/search" || url.pathname === "/api/v1/brain/search") lookups.push(url);
      });
      const requests = pathRequests(page);
      await page.setViewportSize({ width: 1440, height: 900 });
      await persistRepresentation(page, "graph");
      await page.goto(
        `/?workspace=${encodeURIComponent(workspace!.id)}&node=${encodeURIComponent(from.uid)}&kind=${from.kind}`,
      );
      const details = page.getByTestId("detail-panel");
      await expect(details.getByText("releaseA").first()).toBeVisible({ timeout: 15_000 });
      await details.getByRole("group", { name: "Node actions" }).first().getByRole("button", { name: /^Path$/ }).click();
      const dialog = page.getByRole("dialog", { name: "Find path" });
      lookups.length = 0;
      await dialog.getByRole("textbox", { name: "Path target" }).fill("releaseC");
      await dialog.getByRole("button", { name: "Find" }).click();
      await expect(page.getByRole("button", { name: /^Path 1:/ }).first()).toBeVisible({ timeout: 10_000 });
      expect(requests).toEqual([`${from.uid}/${target.uid}`]);
      expect(lookups.map((url) => url.pathname)).toEqual(["/api/v1/brain/search"]);
      expect(lookups[0].searchParams.get("workspace")).toBe(workspace!.id);
    });

    test("counterweight: a full uid still finds the path", async ({ page, request }) => {
      const target = await findSymbol(request, "releaseC");
      const requests = pathRequests(page);
      const { dialog, from } = await openPathDialog(page, request);
      await dialog.getByRole("textbox", { name: "Path target" }).fill(target.uid);
      await dialog.getByRole("button", { name: "Find" }).click();
      await expect(page.getByRole("button", { name: /^Path 1:/ }).first()).toBeVisible({ timeout: 10_000 });
      expect(requests).toEqual([`${from.uid}/${target.uid}`]);
    });
  });
});
