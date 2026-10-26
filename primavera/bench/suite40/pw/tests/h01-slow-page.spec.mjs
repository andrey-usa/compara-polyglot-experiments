import { test, expect, SHOP } from "../fixtures.mjs";

test("h01 — content is usable before the load event finishes", async ({ page }) => {
  // /slow serves an image that never finishes; waitUntil commit must not
  // hang on the load event.
  await page.goto(`${SHOP}/slow`, { waitUntil: "commit" });
  await expect(page.getByText("DOM is ready.")).toBeVisible();
});
