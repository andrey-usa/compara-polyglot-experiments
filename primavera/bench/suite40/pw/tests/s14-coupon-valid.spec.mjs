import { test, expect, SHOP, ASSERT_MS, acceptCookies } from "../fixtures.mjs";

test("s14 — a valid coupon lowers the total", async ({ page }) => {
  await acceptCookies(page);
  await page.goto(`${SHOP}/shop/p/101`, { waitUntil: "commit" });
  await page.locator("#detail-add").click();
  await expect(page.locator("#cart-count")).toHaveText("1");
  await page.goto(`${SHOP}/cart`, { waitUntil: "commit" });
  // The coupon widget is a web component with an open shadow root;
  // Playwright locators pierce it.
  await page.getByLabel("Coupon code").fill("SAVE10");
  await page.getByRole("button", { name: "Apply coupon" }).click();
  await expect(page.getByText("Coupon SAVE10 applied")).toBeVisible();
  await expect(page.getByText("Discount (SAVE10)")).toBeVisible({ timeout: ASSERT_MS });
});
