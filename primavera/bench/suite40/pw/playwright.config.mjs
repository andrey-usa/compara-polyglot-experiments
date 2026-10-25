// @playwright/test config for the suite40 benchmark port.
// Everything is localhost (shop on 18001, gauntlet on 18201/18202), so no
// proxy is needed. Drives the same headless-shell Chromium as the primavera
// leg; no browser is downloaded. Assertions keep a 30 s budget.
import { defineConfig } from "@playwright/test";

const SHELL =
  process.env.CHROME_SHELL ||
  process.env.HOME + "/.cache/ms-playwright/chromium_headless_shell-1248/chrome-headless-shell-linux64/chrome-headless-shell";

export default defineConfig({
  testDir: "./tests",
  timeout: 180000,
  expect: { timeout: 30000 },
  retries: 0,
  reporter: [["list"]],
  use: {
    ignoreHTTPSErrors: true,
    acceptDownloads: true,
  },
  projects: [
    {
      name: "chromium",
      use: {
        browserName: "chromium",
        launchOptions: {
          executablePath: SHELL,
          args: ["--no-sandbox"],
        },
      },
    },
  ],
});
