import { test, expect, SHOP, acceptCookies } from "../fixtures.mjs";

test("s10 — the cart shows line items and the subtotal", async ({ page }) => {
  await acceptCookies(page);
  await page.goto(`${SHOP}/shop/p/101`, { waitUntil: "commit" });
  await page.locator("#qty").fill("3");
  await page.locator("#detail-add").click();
  await expect(page.locator("#cart-count")).toHaveText("3");
  await page.goto(`${SHOP}/cart`, { waitUntil: "commit" });
  await expect(page.getByText("Brass Widget")).toBeVisible();
  await expect(page.locator("#subtotal")).toHaveText("$180.96");
  await expect(page.locator("#qty-101")).toHaveValue("3");
});
