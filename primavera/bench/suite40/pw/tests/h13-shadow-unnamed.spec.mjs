import { test, expect, GAUNTLET } from "../fixtures.mjs";

test("h13 — clicking the nameless button in an open shadow root", async ({ page }) => {
  await page.goto(`${GAUNTLET}/shadow-open-unnamed`, { waitUntil: "commit" });
  // Icon-only, aria-hidden button inside the open shadow root of #host.
  await page.locator("#host").locator("button").click();
  await expect(page.locator("#out")).toHaveText("played");
});
