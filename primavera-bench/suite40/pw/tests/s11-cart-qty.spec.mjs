import { test, expect, SHOP, ASSERT_MS, acceptCookies } from "../fixtures.mjs";

test("s11 — changing the quantity updates the subtotal", async ({ page }) => {
  await acceptCookies(page);
  await page.goto(`${SHOP}/shop/p/101`, { waitUntil: "commit" });
  await page.locator("#detail-add").click();
  await expect(page.locator("#cart-count")).toHaveText("1");
  await page.goto(`${SHOP}/cart`, { waitUntil: "commit" });
  await page.locator("#qty-101").fill("4");
  // Playwright's fill doesn't blur, so the site's change handler hasn't run:
  // click the heading to move focus away, like a user would.
  await page.getByRole("heading", { name: "Your cart" }).click();
  // The change re-posts and reloads the page.
  await expect(page.locator("#subtotal")).toHaveText("$241.28", { timeout: ASSERT_MS });
});
