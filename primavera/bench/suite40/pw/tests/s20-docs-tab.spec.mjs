import { test, expect, acceptCookies } from "../fixtures.mjs";

test("s20 — the docs link opens in a new tab", async ({ page, context }) => {
  await acceptCookies(page);
  const [docs] = await Promise.all([
    context.waitForEvent("page"),
    page.getByRole("link", { name: "Docs" }).click(),
  ]);
  await expect(docs).toHaveURL(/\/docs/);
  await expect(docs.getByText("Returns FAQ")).toBeVisible();
});
