import { test, expect, SHOP, acceptCookies } from "../fixtures.mjs";

test("s22 — the support form requires category and message", async ({ page }) => {
  await acceptCookies(page);
  await page.goto(`${SHOP}/support`, { waitUntil: "commit" });
  const widget = page.frameLocator('iframe[title="Support request form"]');
  await widget.locator("#send").click();
  await expect(widget.locator("#err")).toHaveText("Category and message are required.");
});
