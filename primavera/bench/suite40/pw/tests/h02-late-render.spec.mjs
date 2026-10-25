import { test, expect, GAUNTLET, ASSERT_MS } from "../fixtures.mjs";

test("h02 — waiting for the late render before continuing", async ({ page }) => {
  await page.goto(`${GAUNTLET}/late-render`, { waitUntil: "commit" });
  await expect(page.locator("#ready")).toBeVisible({ timeout: ASSERT_MS });
  await page.locator("#go").click();
  await expect(page.getByText("Done")).toBeVisible();
});
