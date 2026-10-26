import { test, expect, acceptCookies, signIn } from "../fixtures.mjs";

test("s18 — paging through the order history", async ({ page }) => {
  await acceptCookies(page);
  await signIn(page);
  await expect(page.locator("#pageinfo")).toHaveText("Page 1 of 3");
  await page.locator("#next").click();
  await expect(page.locator("#pageinfo")).toHaveText("Page 2 of 3");
  await expect(page.locator("#orders tr")).toHaveCount(10);
  await page.locator("#prev").click();
  await expect(page.locator("#pageinfo")).toHaveText("Page 1 of 3");
});
