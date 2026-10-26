import { test, expect, acceptCookies } from "../fixtures.mjs";

test("s06 — a search with no matches says so", async ({ page }) => {
  await acceptCookies(page);
  const search = page.getByLabel("Search products");
  await search.fill("zzz-no-such-product");
  await search.press("Enter");
  await expect(page.getByRole("status")).toHaveText("No products match.");
  await expect(page.locator(".card")).toHaveCount(0);
});
