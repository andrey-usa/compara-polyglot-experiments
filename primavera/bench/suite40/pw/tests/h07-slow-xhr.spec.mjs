import { test, expect, GAUNTLET, ASSERT_MS } from "../fixtures.mjs";

test("h07 — using the result only after the slow XHR resolves", async ({ page }) => {
  await page.goto(`${GAUNTLET}/slow-xhr`, { waitUntil: "commit" });
  await page.locator("#load").click();
  await expect(page.locator("#use")).toBeEnabled({ timeout: ASSERT_MS });
  await page.locator("#use").click();
  await expect(page.locator("#done")).toHaveText("used: seed-7-result");
});
