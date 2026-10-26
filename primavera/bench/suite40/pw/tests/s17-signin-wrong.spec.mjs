import { test, expect, acceptCookies, signIn } from "../fixtures.mjs";

test("s17 — a wrong password stays on the sign-in page", async ({ page }) => {
  await acceptCookies(page);
  await signIn(page, "demo@acme.test", "wrong");
  await expect(page.getByRole("alert")).toContainText("Wrong email or password");
  await expect(page).toHaveTitle(/Sign in/);
});
