const { defineConfig } = require("@playwright/test");

module.exports = defineConfig({
  testDir: "./tests/browser",
  workers: 1,
  timeout: 45_000,
  expect: { timeout: 10_000 },
  use: {
    baseURL: "http://127.0.0.1:18770",
    launchOptions: process.env.ERISDB_CHROMIUM ? { executablePath: process.env.ERISDB_CHROMIUM } : {},
    trace: "retain-on-failure",
  },
  webServer: {
    command: "node tests/browser/server.cjs",
    url: "http://127.0.0.1:18770/ready",
    timeout: 900_000, // Includes a cold Rust build on a fresh checkout.
    reuseExistingServer: false,
  },
});
