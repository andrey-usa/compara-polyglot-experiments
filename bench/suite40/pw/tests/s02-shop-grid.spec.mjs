import { test, expect, SHOP, acceptCookies } from "../fixtures.mjs";

test("s02 — the shop lists products after the delayed fetch", async ({ page }) => {
  await acceptCookies(page);
  await expect(page.getByRole("status")).toHaveText("Showing 12 of 60 products");
  await expect(page.locator(".card")).toHaveCount(12);
});
