import { test, expect, GAUNTLET } from "../fixtures.mjs";

test("h03 — clicking the live mount of a re-mounted button", async ({ page }) => {
  await page.goto(`${GAUNTLET}/remount`, { waitUntil: "commit" });
  await page.locator("#target").click();
  await expect(page.locator("#pos")).toContainText("committed gen");
});
