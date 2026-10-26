import { test, expect, acceptCookies, signIn } from "../fixtures.mjs";

test("s16 — signing in shows the order history", async ({ page }) => {
  await acceptCookies(page);
  await signIn(page);
  await expect(page).toHaveURL(/\/account$/);
  await expect(page.getByText("Signed in as")).toBeVisible();
  await expect(page.getByText("demo@acme.test")).toBeVisible();
  await expect(page.locator("#pageinfo")).toHaveText("Page 1 of 3");
  const cookies = await page.context().cookies();
  expect(cookies.some((c) => c.name === "sid")).toBe(true);
});
