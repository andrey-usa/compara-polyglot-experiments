import { test, expect, SHOP, acceptCookies } from "../fixtures.mjs";

test("s21 — the returns FAQ opens", async ({ page }) => {
  await acceptCookies(page);
  await page.goto(`${SHOP}/docs`, { waitUntil: "commit" });
  await page.getByText("Returns FAQ").click();
  await expect(page.getByText("37 days from delivery")).toBeVisible();
  await expect(page.getByText("8% applies to opened items")).toBeVisible();
  await expect(page.locator("#returns-faq")).toHaveAttribute("open", "");
});
