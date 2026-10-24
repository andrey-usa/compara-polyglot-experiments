import { test, expect, SHOP, acceptCookies } from "../fixtures.mjs";

test("s25 — placing an order end to end", async ({ page }) => {
  await acceptCookies(page);
  await page.goto(`${SHOP}/shop/p/101`, { waitUntil: "commit" });
  await page.locator("#qty").fill("3");
  await page.locator("#detail-add").click();
  await expect(page.locator("#cart-count")).toHaveText("3");
  await page.goto(`${SHOP}/cart`, { waitUntil: "commit" });
  await page.locator("#checkout").click();
  await expect(page).toHaveURL(/\/checkout/);
  await page.locator("#name").fill("Ada Lovelace");
  await page.locator("#country").selectOption("United States");
  await page.locator('input[name="speed"][value="standard"]').click();
  await page.locator("#continue").click();
  await expect(page.locator("#step2")).toBeVisible();
  await expect(page.locator("#summary")).toContainText("$185.96");
  const pay = page.frameLocator("#payframe");
  await pay.locator("#card").fill("4242424242424242");
  await pay.locator("#exp").fill("12/30");
  await pay.locator("#cvc").fill("123");
  const orderPost = page.waitForResponse(
    (r) => r.url().includes("/api/orders") && r.request().method() === "POST");
  page.once("dialog", (dialog) => dialog.accept());
  await page.locator("#place").click();
  const resp = await orderPost;
  expect(resp.status()).toBe(200);
  await expect(page.locator("#order-id")).toHaveText(/^A-20\d{2}$/);
  await expect(page.getByText("Total charged: $185.96")).toBeVisible();
});
