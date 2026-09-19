import { defineConfig } from "@playwright/test";
export default defineConfig({
  testDir: "./tests",
  timeout: 30000,
  fullyParallel: true,
  workers: 3,
  reporter: "list",
  outputDir: "/tmp/hive-ui-test-results",
  use: {
    baseURL: "http://127.0.0.1:18081",
    channel: "chrome",
    trace: "retain-on-failure",
  },
  webServer: {
    command: "node tests/serve.mjs",
    url: "http://127.0.0.1:18081",
    reuseExistingServer: false,
  },
});
