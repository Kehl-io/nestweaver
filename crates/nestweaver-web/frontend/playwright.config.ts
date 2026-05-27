import { defineConfig } from "@playwright/test";

export default defineConfig({
  testDir: "./e2e",
  timeout: 30_000,
  retries: 1,
  use: {
    baseURL: "http://localhost:3000",
    trace: "on-first-retry",
    screenshot: "only-on-failure",
  },
  projects: [
    {
      name: "chromium",
      use: { browserName: "chromium" },
    },
  ],
  webServer: {
    command: process.env.CI
      ? `${process.env.GITHUB_WORKSPACE}/target/debug/nestweaver ui --db /tmp/test.lbug --port 3000 --no-open`
      : "cargo run --manifest-path ../../../../Cargo.toml -- ui --db ./e2e/fixtures/test.lbug --port 3000 --no-open",
    port: 3000,
    timeout: 120_000,
    reuseExistingServer: !process.env.CI,
  },
});
