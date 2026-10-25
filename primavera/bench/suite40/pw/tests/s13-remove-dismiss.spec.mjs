import { test, expect, SHOP, acceptCookies } from "../fixtures.mjs";

test("s13 — dismissing the remove confirm keeps the item", async ({ page }) => {
  await acceptCookies(page);
  await page.goto(`${SHOP}/shop/p/101`, { waitUntil: "commit" });
  await page.locator("#detail-add").click();
  await expect(page.locator("#cart-count")).toHaveText("1");
  await page.goto(`${SHOP}/cart`, { waitUntil: "commit" });
  page.once("dialog", (dialog) => dialog.dismiss());
  await page.locator('[data-remove="101"]').click();
  await expect(page.getByText("Brass Widget")).toBeVisible();
  await expect(page.locator("#cart-count")).toHaveText("1");
});
