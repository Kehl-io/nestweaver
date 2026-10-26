import { expect, test, type Page } from "@playwright/test";

// These browser tests exercise the real login/page/request integration with a
// mocked API. Backend grant semantics remain covered by the Rust device tests.
const fixtureToken = "fixture-admin-device-test";

async function login(page: Page): Promise<void> {
  await page.route("**/admin/api/status", async (route) => {
    const authenticated =
      route.request().headers().authorization === `Bearer ${fixtureToken}`;
    await route.fulfill({
      status: authenticated ? 200 : 401,
      json: authenticated ? { instance_id: "device-approval-fixture" } : {},
    });
  });
  await page.goto("/admin/login");
  await page.getByPlaceholder("nw_admin_...").fill(fixtureToken);
  await page.getByRole("button", { name: "Sign In" }).click();
  await expect(page.getByRole("heading", { name: /Server Overview/ })).toBeVisible();
}

test("device approval sends the session bearer to the auth route for independent grants", async ({ page }) => {
  const approved: string[] = [];
  let wrongRouteCalls = 0;
  await page.route("**/admin/api/auth/device/approve", async (route) => {
    wrongRouteCalls += 1;
    await route.fulfill({ status: 404 });
  });
  await page.route((url) => url.pathname === "/auth/device/approve", async (route) => {
    const request = route.request();
    expect(new URL(request.url()).pathname).toBe("/auth/device/approve");
    expect(request.method()).toBe("POST");
    expect(request.headers().authorization).toBe(`Bearer ${fixtureToken}`);
    expect(request.headers()["content-type"]).toBe("application/json");
    approved.push(request.postDataJSON().user_code);
    await route.fulfill({ json: { message: "device approved" } });
  });
  await login(page);
  for (const code of ["FIRST123", "NEXT4567"]) {
    await page.goto(`/admin/device-approve?user_code=${code}`);
    await page.getByRole("button", { name: "Approve Device", exact: true }).click();
    await expect(page.getByText("device approved", { exact: true })).toBeVisible();
  }
  expect(approved).toEqual(["FIRST123", "NEXT4567"]);
  expect(wrongRouteCalls).toBe(0);
});

test("device approval follows shared session-expiry handling on unauthorized response", async ({ page }) => {
  await page.route((url) => url.pathname === "/auth/device/approve", async (route) => {
    expect(route.request().headers().authorization).toBe(`Bearer ${fixtureToken}`);
    await route.fulfill({ status: 401, body: "admin token required" });
  });
  await login(page);
  await page.goto("/admin/device-approve?user_code=EXPIRED1");
  await page.getByRole("button", { name: "Approve Device", exact: true }).click();
  await expect(page).toHaveURL(/\/admin\/login$/);
  expect(await page.evaluate(() => sessionStorage.getItem("admin_token"))).toBeNull();
});

for (const code of ["UNKNOWN1", "EXPIRED2"]) {
  test(`authenticated ${code} refusal stays an error without clearing the valid session`, async ({ page }) => {
    await page.route((url) => url.pathname === "/auth/device/approve", async (route) => {
      expect(route.request().headers().authorization).toBe(`Bearer ${fixtureToken}`);
      expect(route.request().postDataJSON()).toEqual({ user_code: code });
      await route.fulfill({ status: 404, body: "no pending device with that code" });
    });
    await login(page);
    await page.goto(`/admin/device-approve?user_code=${code}`);
    await page.getByRole("button", { name: "Approve Device", exact: true }).click();
    await expect(page.getByText("404 Not Found", { exact: true })).toBeVisible();
    await expect(page.getByRole("button", { name: "Approve Device", exact: true })).toBeEnabled();
    expect(await page.evaluate(() => sessionStorage.getItem("admin_token"))).toBe(fixtureToken);
  });
}

test("shared authenticated admin GET, POST and DELETE keep the admin API prefix", async ({ page }) => {
  const mutations: string[] = [];
  let present = false;
  await page.route("**/admin/api/repos**", async (route) => {
    const request = route.request();
    expect(request.headers().authorization).toBe(`Bearer ${fixtureToken}`);
    const path = new URL(request.url()).pathname;
    if (request.method() === "GET") {
      expect(path).toBe("/admin/api/repos");
      await route.fulfill({
        json: present ? [{ id: "fixture-repo", url: "https://example.test/repo", status: "indexed" }] : [],
      });
    } else if (request.method() === "POST") {
      expect(path).toBe("/admin/api/repos");
      expect(request.postDataJSON()).toEqual({ url: "https://example.test/repo" });
      mutations.push("POST");
      present = true;
      await route.fulfill({ json: { message: "added" } });
    } else if (request.method() === "DELETE") {
      expect(path).toBe("/admin/api/repos/fixture-repo");
      mutations.push("DELETE");
      present = false;
      await route.fulfill({ status: 204 });
    } else {
      throw new Error(`Unexpected fixture method ${request.method()}`);
    }
  });
  await login(page);
  await page.goto("/admin/repos");
  await expect(page.getByText("No repositories configured.")).toBeVisible();
  await page.getByPlaceholder("https://github.com/org/repo").fill("https://example.test/repo");
  await page.getByRole("button", { name: "Add Repo", exact: true }).click();
  await expect(page.getByRole("cell", { name: "fixture-repo", exact: true })).toBeVisible();
  page.on("dialog", (dialog) => dialog.accept());
  await page.getByRole("button", { name: "Remove", exact: true }).click();
  await expect(page.getByText("No repositories configured.")).toBeVisible();
  expect(mutations).toEqual(["POST", "DELETE"]);
});
