import { test, expect, SHOP, acceptCookies } from "../fixtures.mjs";

test("s09 — the detail page adds the chosen quantity", async ({ page }) => {
  await acceptCookies(page);
  await page.goto(`${SHOP}/shop/p/101`, { waitUntil: "commit" });
  await page.locator("#qty").fill("3");
  await page.locator("#detail-add").click();
  await expect(page.locator("#cart-count")).toHaveText("3");
});
