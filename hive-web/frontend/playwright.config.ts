import { defineConfig } from "@playwright/test";
export default defineConfig({
  testDir: "./tests",
  timeout: 30000,
  fullyParallel: true,
  // A stray test.only would silently skip the rest of the suite in CI.
  forbidOnly: !!process.env.CI,
  workers: 3,
  reporter: "list",
  outputDir: "/tmp/hive-ui-test-results",
  use: {
    baseURL: "http://127.0.0.1:18081",
    // CI uses Playwright's pinned Chromium; locally, the installed Chrome.
    channel: process.env.CI ? undefined : "chrome",
    trace: "retain-on-failure",
  },
  webServer: {
    command: "node tests/serve.mjs",
    url: "http://127.0.0.1:18081",
    reuseExistingServer: false,
  },
});
