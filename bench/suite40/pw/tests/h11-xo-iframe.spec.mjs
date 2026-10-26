import { test, expect, GAUNTLET } from "../fixtures.mjs";

test("h11 — filling the cross-origin payment frame", async ({ page }) => {
  await page.goto(`${GAUNTLET}/xo-iframe`, { waitUntil: "commit" });
  const pay = page.frameLocator("#pay");
  await pay.locator("#cardholder").fill("Ada Lovelace");
  await pay.locator("#card").fill("4242424242424242");
  await pay.getByRole("button", { name: "Pay" }).click();
  await expect(pay.locator("#out")).toHaveText("paid");
});
