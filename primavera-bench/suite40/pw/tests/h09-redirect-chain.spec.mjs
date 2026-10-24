import { test, expect, GAUNTLET, ASSERT_MS } from "../fixtures.mjs";

test("h09 — following the redirect chain hop by hop", async ({ page }) => {
  await page.goto(`${GAUNTLET}/redir/1`, { waitUntil: "commit" });
  await expect.poll(() => page.url(), { timeout: ASSERT_MS }).toContain("/redir/final");
  await page.locator("#go").click();
  await expect(page.getByText("finished")).toBeVisible();
});
