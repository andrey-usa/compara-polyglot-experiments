import { test, expect, GAUNTLET } from "../fixtures.mjs";

test("h04 — clicking after the transition settles", async ({ page }) => {
  await page.goto(`${GAUNTLET}/transition-transform`, { waitUntil: "commit" });
  await page.waitForTimeout(2000);
  await page.locator("#go").click();
  await expect(page.locator("#out")).toHaveText("clicked");
});
