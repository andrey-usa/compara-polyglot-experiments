import { defineConfig } from '@playwright/test';

// The same 7 scenarios as primavera/examples/bench-site, on the runner's
// own Chrome (no browser download), one browser context per test.
export default defineConfig({
  testDir: './tests',
  fullyParallel: false,
  retries: 0,
  reporter: 'line',
  use: {
    baseURL: process.env.BASE_URL,
    channel: process.env.PW_CHANNEL ?? 'chrome',
    headless: true,
  },
});
