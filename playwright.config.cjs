const { defineConfig } = require("@playwright/test");

module.exports = defineConfig({
  testDir: "./tests/browser",
  workers: 1,
  timeout: 300_000,
  expect: { timeout: 10_000 },
  use: {
    baseURL: "http://127.0.0.1:18770",
    launchOptions: process.env.ERISDB_CHROMIUM ? { executablePath: process.env.ERISDB_CHROMIUM } : {},
    trace: "retain-on-failure",
  },
  webServer: {
    command: "node tests/browser/server.cjs",
    url: "http://127.0.0.1:18770/ready",
    timeout: 1_800_000, // Includes cold Rust builds of ErisDB, ErisOverlayNetwork and the relay.
    reuseExistingServer: false,
    // The harness removes its database container and scratch directories as it stops.
    gracefulShutdown: { signal: "SIGTERM", timeout: 30_000 },
  },
});
