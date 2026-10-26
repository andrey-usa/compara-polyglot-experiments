import { test, expect, acceptCookies } from "../fixtures.mjs";

test("s08 — adding a product from the grid counts it in the cart", async ({ page }) => {
  await acceptCookies(page);
  await expect(page.locator(".card")).toHaveCount(12);
  await page.getByText("Add to cart").first().click();
  await expect(page.locator("#cart-count")).toHaveText("1");
  await expect(page.getByText("Added to cart")).toBeVisible();
});
