import { test, expect, SHOP } from "../fixtures.mjs";

test("s01 — accepting the cookie banner dismisses it", async ({ page }) => {
  await page.goto(`${SHOP}/shop`, { waitUntil: "commit" });
  await expect(page.locator("#cookie")).toBeVisible();
  await page.locator("#cookie-accept").click();
  await expect(page.locator("#cookie")).toBeHidden();
  await page.goto(`${SHOP}/cart`, { waitUntil: "commit" });
  await expect(page.locator("#cookie")).toBeHidden();
});
