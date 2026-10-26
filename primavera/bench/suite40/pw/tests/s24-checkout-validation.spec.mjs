import { test, expect, SHOP, acceptCookies } from "../fixtures.mjs";

test("s24 — checkout validates shipping and card inputs", async ({ page }) => {
  await acceptCookies(page);
  await page.goto(`${SHOP}/shop/p/101`, { waitUntil: "commit" });
  await page.locator("#detail-add").click();
  await expect(page.locator("#cart-count")).toHaveText("1");
  await page.goto(`${SHOP}/checkout`, { waitUntil: "commit" });
  await page.locator("#continue").click();
  const err1 = page.locator("#err1");
  await expect(err1).toContainText("Full name is required.");
  await expect(err1).toContainText("Choose a country.");
  await expect(err1).toContainText("Choose a shipping speed.");
  await page.locator("#name").fill("Ada Lovelace");
  await page.locator("#continue").click();
  await expect(err1).toHaveText("Choose a country. Choose a shipping speed.");
  await page.locator("#country").selectOption("Canada");
  await page.locator('input[name="speed"][value="express"]').click();
  await page.locator("#continue").click();
  await expect(page.locator("#step2")).toBeVisible();
  await page.locator("#place").click();
  await expect(page.locator("#err2")).toContainText("Card details are incomplete");
});
