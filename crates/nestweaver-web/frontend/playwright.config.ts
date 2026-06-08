import { defineConfig } from "@playwright/test";

const isCi = Boolean(process.env.CI);

export default defineConfig({
  testDir: "./e2e",
  timeout: 30_000,
  retries: 1,
  use: {
    baseURL: isCi ? "http://localhost:3000" : "http://localhost:5173",
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
    command: isCi
      ? `${process.env.GITHUB_WORKSPACE}/target/debug/nestweaver ui --db /tmp/test.lbug --port 3000 --no-open`
      : "sh -c 'set -e; cargo run --manifest-path ../../../Cargo.toml -- --no-daemon index --repo ../../../testdata/js --db /tmp/nestweaver-e2e.lbug; cargo run --manifest-path ../../../Cargo.toml -- ui --db /tmp/nestweaver-e2e.lbug --port 3000 --no-open & api_pid=$!; trap \"kill $api_pid 2>/dev/null || true\" EXIT; i=0; until curl -fsS http://127.0.0.1:3000/api/v1/health >/dev/null 2>&1; do i=$((i + 1)); if [ \"$i\" -ge 120 ]; then echo \"API did not become ready\"; exit 1; fi; sleep 0.25; done; npm run dev -- --host 127.0.0.1 --port 5173'",
    port: isCi ? 3000 : 5173,
    timeout: 120_000,
    reuseExistingServer: !isCi,
  },
});
