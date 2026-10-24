import { test, expect, SHOP, acceptCookies } from "../fixtures.mjs";

test("s12 — removing an item after confirming empties the cart", async ({ page }) => {
  await acceptCookies(page);
  await page.goto(`${SHOP}/shop/p/101`, { waitUntil: "commit" });
  await page.locator("#detail-add").click();
  await expect(page.locator("#cart-count")).toHaveText("1");
  await page.goto(`${SHOP}/cart`, { waitUntil: "commit" });
  page.once("dialog", (dialog) => dialog.accept());
  await page.locator('[data-remove="101"]').click();
  await expect(page.getByText("Your cart is empty.")).toBeVisible();
  await expect(page.locator("#cart-count")).toHaveText("0");
});
