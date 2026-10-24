import { test, expect, GAUNTLET } from "../fixtures.mjs";

test("h12 — clicking the named button in an open shadow root", async ({ page }) => {
  await page.goto(`${GAUNTLET}/shadow-open-named`, { waitUntil: "commit" });
  await page.getByText("Pay now").click();
  await expect(page.locator("#out")).toHaveText("paid");
});
