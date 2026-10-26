import { test, expect, GAUNTLET } from "../fixtures.mjs";

test("h15 — selecting every option in a multi-select", async ({ page }) => {
  await page.goto(`${GAUNTLET}/select-multiple`, { waitUntil: "commit" });
  await page.locator("#ms").selectOption(["a", "b", "c"]);
  await page.locator("#go").click();
  await expect(page.locator("#out")).toHaveText("picked a,b,c");
});
