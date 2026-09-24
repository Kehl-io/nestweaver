import { expect, test, type Route } from "@playwright/test";

// nw-648: the live brain had two vaults and ~1918 notes, but the Notes
// explorer showed "BRAIN 991" and never listed the second vault — it loaded
// one unfiltered 1000-row page and counted rows per vault. The API is mocked
// so the fixture can hold a vault larger than one page beside a second vault.

const PAGE = 1000;
const BRAIN = { uid: "vlt:fixture:brain", name: "brain", total: PAGE + 3 };
const DOCS = { uid: "vlt:fixture:docs", name: "kehl-craft-docs", total: 4 };

function note(vault: { uid: string; name: string }, i: number) {
  const id = String(i).padStart(4, "0");
  return {
    uid: `note:${vault.name}:${id}`,
    vault_uid: vault.uid,
    file_path: `n${id}.md`,
    title: `${vault.name} note ${id}`,
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
  const offset = Number(url.searchParams.get("offset") ?? "0");
  const limit = Number(url.searchParams.get("limit") ?? "20");
  // An unfiltered request is answered the way the old server did: one page,
  // all from the first vault.
  const vault = vaultUid === DOCS.uid ? DOCS : BRAIN;
  const total = vaultUid ? vault.total : BRAIN.total + DOCS.total;
  const end = Math.min(vault.total, offset + limit);
  const rows = [];
  for (let i = offset; i < end; i += 1) rows.push(note(vault, i));
  return route.fulfill({
    status: 200,
    contentType: "application/json",
    headers: { "x-total-count": String(total) },
    body: JSON.stringify(rows),
  });
}

test.use({ viewport: { width: 1440, height: 900 } });

test("notes explorer lists every vault with its true count and pages the rest", async ({ page }) => {
  await page.route("**/api/v1/brain/vaults", (route) =>
    route.fulfill({
      status: 200,
      contentType: "application/json",
      body: JSON.stringify(
        [BRAIN, DOCS].map((v) => ({
          uid: v.uid,
          name: v.name,
          root_path: `/fixture/${v.name}`,
          instance_id: "fixture",
          note_count: v.total,
        })),
      ),
    }),
  );
  await page.route("**/api/v1/brain/tags", (route) =>
    route.fulfill({ status: 200, contentType: "application/json", body: "[]" }),
  );
  await page.route("**/api/v1/brain/notes?**", fulfillNotes);

  await page.goto("/");
  const explorer = page.getByTestId("explorer-panel");
  await explorer.getByRole("button", { name: "Notes", exact: true }).click();

  const brainHeader = explorer.getByRole("button", { name: /^▾\s*brain\s*\d+$/ });
  const docsHeader = explorer.getByRole("button", { name: /^▾\s*kehl-craft-docs\s*\d+$/ });
  await expect(brainHeader).toContainText(String(BRAIN.total));
  await expect(docsHeader).toContainText(String(DOCS.total));
  const docs = explorer.getByTestId("notes-vault-kehl-craft-docs");
  await expect(docs.getByText("kehl-craft-docs note 0003")).toBeVisible();

  // The larger vault discloses what is not listed yet, then pages it in.
  const brain = explorer.getByTestId("notes-vault-brain");
  await expect(brain.getByTestId("notes-vault-truncated")).toContainText(
    `Showing ${PAGE} of ${BRAIN.total} notes`,
  );
  await brain.getByRole("button", { name: "Load more" }).click();
  await expect(brain.getByText("brain note 1002")).toBeAttached();
  await expect(brain.getByTestId("notes-vault-truncated")).toHaveCount(0);
});

// Counterweight: a single vault under one page looks as it always did — its
// count, its notes, and no truncation line.
test("a single small vault lists all its notes with no truncation disclosure", async ({ page }) => {
  const SMALL = { uid: "vlt:fixture:small", name: "small", total: 5 };
  await page.route("**/api/v1/brain/vaults", (route) =>
    route.fulfill({
      status: 200,
      contentType: "application/json",
      body: JSON.stringify([
        { uid: SMALL.uid, name: SMALL.name, root_path: "/fixture/small", instance_id: "fixture", note_count: SMALL.total },
      ]),
    }),
  );
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
  await expect(small.getByTestId("notes-vault-truncated")).toHaveCount(0);
});
