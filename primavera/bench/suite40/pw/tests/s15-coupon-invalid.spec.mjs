import { test, expect, SHOP, acceptCookies } from "../fixtures.mjs";

test("s15 — an invalid coupon is rejected", async ({ page }) => {
  await acceptCookies(page);
  await page.goto(`${SHOP}/shop/p/101`, { waitUntil: "commit" });
  await page.locator("#detail-add").click();
  await expect(page.locator("#cart-count")).toHaveText("1");
  await page.goto(`${SHOP}/cart`, { waitUntil: "commit" });
  await page.getByLabel("Coupon code").fill("BOGUS");
  await page.getByRole("button", { name: "Apply coupon" }).click();
  await expect(page.getByText("Invalid coupon")).toBeVisible();
  await expect(page.getByText("Discount (BOGUS)")).toBeHidden();
});
