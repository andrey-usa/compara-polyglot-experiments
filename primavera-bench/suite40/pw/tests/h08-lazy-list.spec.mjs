import { test, expect, GAUNTLET, ASSERT_MS, gauntletVerdict } from "../fixtures.mjs";

test("h08 — scrolling the lazy list to item 45", async ({ page }) => {
  await page.goto(`${GAUNTLET}/lazy-list`, { waitUntil: "commit" });
  const list = page.locator("#list");
  await list.evaluate((el) => { el.scrollTop = 100000; });
  await expect(page.locator("#item-40")).toBeVisible({ timeout: ASSERT_MS });
  await list.evaluate((el) => { el.scrollTop = 100000; });
  await page.locator("#target45").click();
  // The commit is only visible in the page's own event log.
  await expect.poll(() => gauntletVerdict(page, "lazy-list"), { timeout: ASSERT_MS }).toBe(true);
});
