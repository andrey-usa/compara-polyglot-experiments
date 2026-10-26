import { test, expect, GAUNTLET, ASSERT_MS, gauntletVerdict } from "../fixtures.mjs";

test("h10 — submitting the form in nested iframes", async ({ page }) => {
  await page.goto(`${GAUNTLET}/nested-iframes`, { waitUntil: "commit" });
  const inner = page.frameLocator("#outer").frameLocator("#inner");
  await inner.locator("#name").fill("Ada");
  await inner.getByRole("button", { name: "Submit" }).click();
  // The commit must come from depth 2; only the page's own log sees it.
  await expect.poll(() => gauntletVerdict(page, "nested-iframes"), { timeout: ASSERT_MS }).toBe(true);
});
