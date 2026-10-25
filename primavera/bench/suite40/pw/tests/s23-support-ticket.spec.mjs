import { test, expect, SHOP, acceptCookies, UPLOAD_FILE } from "../fixtures.mjs";

test("s23 — filing a support request with an attachment", async ({ page }) => {
  await acceptCookies(page);
  await page.goto(`${SHOP}/support`, { waitUntil: "commit" });
  const widget = page.frameLocator('iframe[title="Support request form"]');
  await widget.locator("#category").selectOption("Billing");
  await widget.locator('input[name="priority"][value="high"]').click();
  await widget.locator("#message").fill("The widget arrived dented.");
  await widget.locator("#attachment").setInputFiles(UPLOAD_FILE);
  await widget.locator("#send").click();
  const body = widget.locator("body");
  await expect(body.getByText("Thanks!")).toBeVisible();
  await expect(body).toContainText(/T-\d+/);
});
