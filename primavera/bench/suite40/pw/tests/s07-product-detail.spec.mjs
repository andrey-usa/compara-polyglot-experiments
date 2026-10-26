import { test, expect, SHOP, acceptCookies } from "../fixtures.mjs";

test("s07 — the product page shows name, price and stock", async ({ page }) => {
  await acceptCookies(page);
  await page.goto(`${SHOP}/shop/p/101`, { waitUntil: "commit" });
  await expect(page.locator("#detail h1")).toHaveText("Brass Widget");
  await expect(page.getByText("Price: $60.32")).toBeVisible();
  await expect(page.locator("#stock")).toHaveText("38");
  await page.locator("#back-to-shop").click();
  await expect(page).toHaveURL(/\/shop$/);
});
