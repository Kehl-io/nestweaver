import { defineConfig } from "@playwright/test";

// The parent fixture owns startup, readiness, evidence, and graceful teardown.
const baseURL = process.env.NESTWEAVER_UI_FIXTURE_URL;
if (!baseURL || !/^http:\/\/127\.0\.0\.1:[1-9][0-9]*$/.test(baseURL)) {
  throw new Error("Run tests/support/release_ui.py with a prebuilt binary; NESTWEAVER_UI_FIXTURE_URL must identify its owned loopback listener.");
}

export default defineConfig({
  testDir: "./e2e",
  timeout: 30_000,
  retries: 1,
  outputDir: process.env.NESTWEAVER_UI_RESULTS_DIR ?? "test-results",
  use: {
    baseURL,
    trace: "retain-on-failure",
    launchOptions: { executablePath: process.env.NESTWEAVER_UI_BROWSER_EXECUTABLE },
    screenshot: "only-on-failure",
  },
  projects: [
    {
      name: "chromium",
      use: { browserName: "chromium" },
    },
  ],
});
