import { test, expect, acceptCookies } from "../fixtures.mjs";

test("s04 — filtering by category shows only that category", async ({ page }) => {
  await acceptCookies(page);
  await page.getByLabel("Category").selectOption("Fasteners");
  await expect(page.getByRole("status")).toHaveText("Showing 12 of 12 products");
  const cats = await page.locator(".card .muted").allTextContents();
  expect(cats).toHaveLength(12);
  expect(cats.every((c) => c.includes("Fasteners"))).toBe(true);
});
