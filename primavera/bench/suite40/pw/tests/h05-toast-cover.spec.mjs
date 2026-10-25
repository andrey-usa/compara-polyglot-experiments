import { test, expect, GAUNTLET } from "../fixtures.mjs";

test("h05 — committing only after the toast dismisses", async ({ page }) => {
  await page.goto(`${GAUNTLET}/toast-cover`, { waitUntil: "commit" });
  await expect(page.locator("#toast")).toBeHidden();
  await page.locator("#go").click();
  await expect(page.locator("#out")).toHaveText("submitted");
});
