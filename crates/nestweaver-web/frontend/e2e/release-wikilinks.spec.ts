import { expect, test } from "@playwright/test";

test("indexed vault wikilinks resolve real aliases and punctuation, focus headings, and refuse missing targets", async ({ page, request }) => {
  test.skip(process.env.NESTWEAVER_UI_WIKILINK_FIXTURE !== "1", "requires release_ui.py --wikilink-fixture");
  const vaultReply = await request.get("/api/v1/brain/vaults");
  expect(vaultReply.ok()).toBe(true);
  const vaults: { uid: string; name: string }[] = await vaultReply.json();
  const vault = vaults.find((entry) => entry.name === "release-wikilinks");
  expect(vault).toBeDefined();
  const notesReply = await request.get(`/api/v1/brain/notes?${new URLSearchParams({ vault: vault!.uid, limit: "1000" })}`);
  expect(notesReply.ok()).toBe(true);
  const notes: { uid: string; title: string }[] = await notesReply.json();
  expect(notes).toHaveLength(2);
  const source = notes.find((entry) => entry.title === "Release Wiki Source");
  const target = notes.find((entry) => entry.title === "Release Wiki Destination");
  expect(source).toBeDefined();
  expect(target).toBeDefined();
  const targetReply = await request.get(`/api/v1/brain/note/${encodeURIComponent(target!.uid)}`);
  expect(targetReply.ok()).toBe(true);
  const targetDetail = await targetReply.json();
  const heading = targetDetail.headings.find((entry: { text: string }) => entry.text === "Usage & examples");
  expect(heading).toBeDefined();
  for (const raw of ["Release Wiki Alias#Usage & examples", "team/A & B %20#Usage & examples"]) {
    const reply = await request.post("/api/v1/brain/wikilink", { data: { source_uid: source!.uid, target: raw } });
    expect(reply.ok(), await reply.text()).toBe(true);
    expect(await reply.json()).toMatchObject({ note_uid: target!.uid, heading_uid: heading.uid, heading_slug: heading.slug });
  }
  const missingReply = await request.post("/api/v1/brain/wikilink", { data: { source_uid: source!.uid, target: "Release Wiki Missing" } });
  expect(missingReply.ok()).toBe(false);
  const refusal = await missingReply.json();
  expect(refusal.error).toBe("wikilink_unresolved");
  expect(refusal.note_uid).toBeUndefined();
  expect(refusal.message).toEqual(expect.any(String));

  // Open through the real catalog; no note or resolver endpoint is intercepted.
  await page.goto("/");
  const explorer = page.getByTestId("explorer-panel");
  await explorer.getByRole("tab", { name: "Notes", exact: true }).click();
  await explorer.getByPlaceholder("Filter notes...").fill("Release Wiki Source");
  const detail = page.getByTestId("detail-panel");
  for (const label of ["Read real alias", "Read real punctuation"]) {
    await explorer.locator("button[data-note-uid]").filter({ hasText: "Release Wiki Source" }).click();
    await expect.poll(() => new URL(page.url()).searchParams.get("node")).toBe(source!.uid);
    const navigation = page.waitForResponse((reply) => new URL(reply.url()).pathname === "/api/v1/brain/wikilink" && reply.request().method() === "POST");
    await detail.getByRole("button", { name: label, exact: true }).click();
    const reply = await navigation;
    expect(reply.ok()).toBe(true);
    expect(await reply.json()).toMatchObject({ note_uid: target!.uid, heading_uid: heading.uid });
    await expect.poll(() => new URL(page.url()).searchParams.get("node")).toBe(target!.uid);
    await expect(detail.getByRole("heading", { name: "Usage & examples", exact: true })).toBeFocused();
    await expect(detail).toContainText("Real indexed destination witness.");
  }
  await explorer.locator("button[data-note-uid]").filter({ hasText: "Release Wiki Source" }).click();
  await expect.poll(() => new URL(page.url()).searchParams.get("node")).toBe(source!.uid);
  const declined = page.waitForResponse((reply) => new URL(reply.url()).pathname === "/api/v1/brain/wikilink" && reply.request().method() === "POST");
  await detail.getByRole("button", { name: "Read real missing", exact: true }).click();
  const reply = await declined;
  expect(reply.ok()).toBe(false);
  expect(await reply.json()).toMatchObject({ error: "wikilink_unresolved", message: refusal.message });
  await expect(page.getByRole("alert").filter({ hasText: refusal.message })).toBeVisible();
  expect(new URL(page.url()).searchParams.get("node")).toBe(source!.uid);
});
