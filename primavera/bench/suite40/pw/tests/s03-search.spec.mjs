import { test, expect, acceptCookies } from "../fixtures.mjs";

test("s03 — search narrows the product list", async ({ page }) => {
  await acceptCookies(page);
  const search = page.getByLabel("Search products");
  await search.fill("valve");
  await search.press("Enter");
  await expect(page.getByRole("status")).toHaveText("Showing 6 of 6 products");
  await expect(page).toHaveURL(/q=valve/);
  const names = await page.locator(".card h3").allTextContents();
  expect(names).toHaveLength(6);
  expect(names.every((n) => n.includes("Valve"))).toBe(true);
});
