import { test, expect, acceptCookies } from "../fixtures.mjs";

test("s19 — hovering the account menu reveals its links", async ({ page }) => {
  await acceptCookies(page);
  await page.getByText("Account").hover();
  await expect(page.getByRole("link", { name: "Sign in" })).toBeVisible();
  await expect(page.getByRole("link", { name: "Orders" })).toBeVisible();
});
