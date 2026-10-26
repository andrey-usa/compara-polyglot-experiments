import { test, expect, acceptCookies } from "../fixtures.mjs";

test("s05 — sorting by price shows the cheapest first", async ({ page }) => {
  await acceptCookies(page);
  await page.getByLabel("Sort by").selectOption("price-asc");
  await expect(page.locator(".card h3").first()).toHaveText("Nickel Spring");
  await expect(page.locator(".card .price").first()).toHaveText("$3.04");
});
