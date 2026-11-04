import { expect, test } from "@playwright/test";

test("a federated search result opens read-only detail without requesting a local source file", async ({ page }) => {
  const hit = { uid: "sym:org:remote-only", name: "RemoteOnlyWitness", kind: "Function", file_path: "remote.rs", start_line: 1 };
  let sourceRequests = 0;
  await page.route("**/api/v1/search?**", (route) => route.fulfill({ json: { results: [hit], _federation: { sources: ["local", "org"], upstreams: [{ source: "org", status: "available" }], partial: false, freshness: "unknown", read_only: true } } }));
  await page.route("**/api/v1/brain/search?**", (route) => route.fulfill({ json: [] }));
  await page.route("**/api/v1/symbol/**", (route) => route.fulfill({ json: { symbol: { ...hit, repo_uid: "repo:org:remote", signature: "fn RemoteOnlyWitness()", summary: null, pagerank_score: 0 }, callers: [], callees: [], _federation: { source: "upstream", freshness: "unknown", read_only: true, source_available: false } } }));
  await page.route("**/api/v1/source?**", async (route) => { sourceRequests += 1; await route.fulfill({ json: { file: hit.file_path, repo: "repo:org:remote", lines: ["STALE LOCAL CONTENT MUST NEVER BECOME REMOTE EVIDENCE"], total_lines: 1 } }); });
  await page.goto("/");
  await expect(page.getByTestId("control-dock")).toBeVisible({ timeout: 15_000 });
  await page.getByTestId("search-input").fill(hit.name);
  const remoteOption = page.getByRole("listbox").getByRole("option").filter({ hasText: hit.name }).first();
  await remoteOption.hover();
  await remoteOption.click();
  const detail = page.getByTestId("detail-panel");
  await expect(detail.getByText(/Upstream result.*freshness unknown.*read only/)).toBeVisible();
  await expect(detail.getByText(/Remote source files are unsupported/)).toBeVisible();
  await expect(page.getByRole("navigation", { name: "Scene breadcrumbs" })).toContainText("remote source unsupported");
  await expect(page.getByText("STALE LOCAL CONTENT MUST NEVER BECOME REMOTE EVIDENCE")).toHaveCount(0);
  expect(sourceRequests).toBe(0);
});

test("search preserves partial upstream outage notices with local and empty results", async ({ page }) => {
  await page.route("**/api/v1/search?**", (route) => {
    const empty = new URL(route.request().url()).searchParams.get("q") === "EmptyOutageWitness";
    return route.fulfill({ json: { results: empty ? [] : [{ uid: "sym:local:run", name: "LocalOutageWitness", kind: "Function", file_path: "local.rs", start_line: 1, federation_source: "local" }], _federation: { sources: ["local"], upstreams: [{ source: "B", status: "unavailable" }], partial: true, freshness: "unknown", read_only: true } } });
  });
  await page.route("**/api/v1/brain/search?**", (route) => route.fulfill({ json: [] }));
  await page.goto("/");
  await expect(page.getByTestId("control-dock")).toBeVisible({ timeout: 15_000 });
  await page.getByTestId("search-input").fill("LocalOutageWitness");
  const notice = page.getByRole("status", { name: "Search availability" });
  await expect(notice).toContainText("Partial search");
  await expect(notice).toContainText("B: unavailable");
  await expect(page.getByRole("listbox")).toContainText("Source: local");
  await page.getByTestId("search-input").fill("EmptyOutageWitness");
  await expect(page.getByRole("listbox")).toContainText("No results");
  await expect(notice).toContainText("Partial search");
  await expect(notice).toContainText("freshness unknown");
});

test("an upstream detail outage is an unavailable error instead of a missing entity", async ({ page }) => {
  const hit = { uid: "sym:org:down", name: "UnavailableDetailWitness", kind: "Function", file_path: "remote.rs", start_line: 1 };
  await page.route("**/api/v1/search?**", (route) => route.fulfill({ json: [hit] }));
  await page.route("**/api/v1/brain/search?**", (route) => route.fulfill({ json: [] }));
  await page.route("**/api/v1/symbol/**", (route) => route.fulfill({ status: 503, json: { error: "Upstream lookup unavailable or permission-rejected (B); absence is unproven." } }));
  await page.goto("/");
  await expect(page.getByTestId("control-dock")).toBeVisible({ timeout: 15_000 });
  await page.getByTestId("search-input").fill(hit.name);
  await page.getByRole("listbox").getByRole("option").filter({ hasText: hit.name }).first().click();
  const detail = page.getByTestId("detail-panel");
  await expect(detail.getByRole("alert")).toContainText("Upstream lookup unavailable");
  await expect(detail).not.toContainText("not in the index");
});
