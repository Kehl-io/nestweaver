import { test, expect } from "@playwright/test";

test.describe("Brain / Notes", () => {
  test("brain status API responds", async ({ request }) => {
    const response = await request.get("/api/v1/brain/status");
    expect(response.ok()).toBeTruthy();
    const body = await response.json();
    expect(body).toHaveProperty("vault_count");
  });

  test("brain search API handles empty query", async ({ request }) => {
    const response = await request.get("/api/v1/brain/search?q=");
    expect(response.status()).toBeLessThan(500);
  });

  test("brain vaults API returns list", async ({ request }) => {
    const response = await request.get("/api/v1/brain/vaults");
    expect(response.ok()).toBeTruthy();
    const body = await response.json();
    expect(Array.isArray(body)).toBe(true);
  });

  test("brain tags API returns list", async ({ request }) => {
    const response = await request.get("/api/v1/brain/tags");
    expect(response.ok()).toBeTruthy();
    const body = await response.json();
    expect(Array.isArray(body)).toBe(true);
  });
});
