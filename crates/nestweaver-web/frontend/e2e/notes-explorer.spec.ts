import { expect, test, type Route } from "@playwright/test";

// nw-648: the live brain had two vaults and ~1918 notes, but the Notes
// explorer showed "BRAIN 991" and never listed the second vault — it loaded
// one unfiltered 1000-row page and counted rows per vault. The API is mocked
// so the fixture can hold a vault larger than one page beside a second vault.

const PAGE = 1000;
// Past the old offset ceiling (offset <= 1000 + one 1000-row page): the
// review found the live main vault at ~1960 notes, right at that edge.
const BRAIN = { uid: "vlt:fixture:brain", name: "brain", total: 2 * PAGE + 3 };
const DOCS = { uid: "vlt:fixture:docs", name: "kehl-craft-docs", total: 4 };

function note(vault: { uid: string; name: string }, i: number) {
  const id = String(i).padStart(4, "0");
  // The docs vault's first note is a template, hidden and disclosed as such.
  const template = vault.uid === DOCS.uid && i === 0;
  return {
    uid: `note:${vault.name}:${id}`,
    vault_uid: vault.uid,
    file_path: template ? `_templates/t${id}.md` : `n${id}.md`,
    title: template ? "{{title}}" : `${vault.name} note ${id}`,
    note_kind: "General",
    word_count: 0,
    content_hash: `h${id}`,
    frontmatter: null,
    created_at: null,
    modified_at: null,
    pagerank_score: null,
  };
}

function fulfillNotes(route: Route) {
  const url = new URL(route.request().url());
  const vaultUid = url.searchParams.get("vault");
  const after = url.searchParams.get("after");
  const limit = Number(url.searchParams.get("limit") ?? "20");
  // An unfiltered request is answered the way the old server did: one page,
  // all from the first vault. `offset` is deliberately not honoured: the
  // explorer must page by cursor.
  const vault = vaultUid === DOCS.uid ? DOCS : BRAIN;
  const total = vaultUid ? vault.total : BRAIN.total + DOCS.total;
  const all = Array.from({ length: vault.total }, (_, i) => note(vault, i));
  const start = after === null ? 0 : all.findIndex((n) => n.uid > after);
  const rows = start < 0 ? [] : all.slice(start, start + limit);
  return route.fulfill({
    status: 200,
    contentType: "application/json",
    headers: { "x-total-count": String(total) },
    body: JSON.stringify(rows),
  });
}

function fulfillVaults(route: Route, vaults: { uid: string; name: string; total: number }[]) {
  return route.fulfill({
    status: 200,
    contentType: "application/json",
    body: JSON.stringify(
      vaults.map((v) => ({
        uid: v.uid,
        name: v.name,
        root_path: `/fixture/${v.name}`,
        instance_id: "fixture",
        note_count: v.total,
      })),
    ),
  });
}

test.use({ viewport: { width: 1440, height: 900 } });

test("notes explorer lists every vault with its true count and pages the rest", async ({ page }) => {
  await page.route("**/api/v1/brain/vaults", (route) => fulfillVaults(route, [BRAIN, DOCS]));
  await page.route("**/api/v1/brain/tags", (route) =>
    route.fulfill({ status: 200, contentType: "application/json", body: "[]" }),
  );
  await page.route("**/api/v1/brain/notes?**", fulfillNotes);

  await page.goto("/");
  const explorer = page.getByTestId("explorer-panel");
  await explorer.getByRole("button", { name: "Notes", exact: true }).click();

  const brainHeader = explorer.getByRole("button", { name: /^▾\s*brain[\s\d]/ });
  const docsHeader = explorer.getByRole("button", { name: /^▾\s*kehl-craft-docs[\s\d]/ });
  await expect(brainHeader).toContainText(String(BRAIN.total));
  await expect(docsHeader).toContainText(String(DOCS.total));
  const docs = explorer.getByTestId("notes-vault-kehl-craft-docs");
  await expect(docs.getByText("kehl-craft-docs note 0003")).toBeVisible();
  await expect(docs.getByTestId("notes-vault-status")).toHaveText("1 template hidden.");

  // The larger vault discloses what is not listed yet, then pages it in by
  // cursor all the way past the old 2000-note ceiling.
  const brain = explorer.getByTestId("notes-vault-brain");
  const status = brain.getByTestId("notes-vault-status");
  await expect(status).toHaveAttribute("aria-live", "polite");
  await expect(status).toContainText(`Showing ${PAGE} of ${BRAIN.total} notes.`);
  const loadMore = brain.getByRole("button", { name: "Load more" });
  await loadMore.click();
  await expect(status).toContainText(`Showing ${2 * PAGE} of ${BRAIN.total} notes.`);
  await loadMore.click();
  await expect(brain.getByText("brain note 2002")).toBeAttached();
  await expect(brain.getByRole("button", { name: "Load more" })).toHaveCount(0);
  await expect(status).toHaveText("");
  await expect(brain.getByRole("listitem")).toHaveCount(BRAIN.total);
  // Focus moved to the first note the last page added, not lost to <body>.
  await expect(brain.locator('[data-note-uid="note:brain:2000"]')).toBeFocused();

  // A title filter reads "matches / total" rather than posing as the size.
  await explorer.getByPlaceholder("Filter notes...").fill("note 0003");
  await expect(docsHeader).toContainText(`1 / ${DOCS.total}`);
  await expect(brainHeader).toContainText(`1 / ${BRAIN.total}`);
});

test("one vault failing to load is shown on that vault, not the whole tab", async ({ page }) => {
  const BROKEN = { uid: "vlt:fixture:broken", name: "broken", total: 7 };
  await page.route("**/api/v1/brain/vaults", (route) => fulfillVaults(route, [DOCS, BROKEN]));
  await page.route("**/api/v1/brain/tags", (route) =>
    route.fulfill({ status: 200, contentType: "application/json", body: "[]" }),
  );
  await page.route("**/api/v1/brain/notes?**", (route) => {
    if (new URL(route.request().url()).searchParams.get("vault") === BROKEN.uid) {
      return route.fulfill({
        status: 500,
        contentType: "application/json",
        body: JSON.stringify({ error: "fixture store failure" }),
      });
    }
    return fulfillNotes(route);
  });

  await page.goto("/");
  const explorer = page.getByTestId("explorer-panel");
  await explorer.getByRole("button", { name: "Notes", exact: true }).click();
  await expect(
    explorer.getByTestId("notes-vault-kehl-craft-docs").getByText("kehl-craft-docs note 0003"),
  ).toBeVisible();
  const broken = explorer.getByTestId("notes-vault-broken");
  await expect(broken.getByRole("alert")).toContainText("fixture store failure");
  await expect(explorer.getByRole("button", { name: /^▾\s*broken[\s\d]/ })).toContainText("7");
});

// Counterweight: a single vault under one page looks as it always did — its
// count, its notes, and no truncation line.
test("a single small vault lists all its notes with no truncation disclosure", async ({ page }) => {
  const SMALL = { uid: "vlt:fixture:small", name: "small", total: 5 };
  await page.route("**/api/v1/brain/vaults", (route) => fulfillVaults(route, [SMALL]));
  await page.route("**/api/v1/brain/tags", (route) =>
    route.fulfill({ status: 200, contentType: "application/json", body: "[]" }),
  );
  await page.route("**/api/v1/brain/notes?**", (route) =>
    route.fulfill({
      status: 200,
      contentType: "application/json",
      headers: { "x-total-count": String(SMALL.total) },
      body: JSON.stringify(Array.from({ length: SMALL.total }, (_, i) => note(SMALL, i))),
    }),
  );

  await page.goto("/");
  const explorer = page.getByTestId("explorer-panel");
  await explorer.getByRole("button", { name: "Notes", exact: true }).click();
  await expect(explorer.getByRole("button", { name: /^▾\s*small\s*\d+$/ })).toContainText("5");
  const small = explorer.getByTestId("notes-vault-small");
  await expect(small.getByRole("listitem")).toHaveCount(SMALL.total);
  await expect(small.getByTestId("notes-vault-status")).toHaveText("");
  await expect(small.getByRole("button", { name: "Load more" })).toHaveCount(0);
});
