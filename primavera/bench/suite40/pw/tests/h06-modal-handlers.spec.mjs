import { test, expect, GAUNTLET } from "../fixtures.mjs";

test("h06 — closing the modal after its handlers bind", async ({ page }) => {
  await page.goto(`${GAUNTLET}/modal-late-handlers`, { waitUntil: "commit" });
  await page.waitForTimeout(2500);
  await page.locator("#close").click();
  await expect(page.locator("#out")).toHaveText("closed");
});
