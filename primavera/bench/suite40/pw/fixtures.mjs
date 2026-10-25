// Shared fixtures for the suite40 Playwright Test port.
// One fresh browser context per test (mirrors primavera's per-scenario
// reset --all isolation); the cookie banner is accepted by a helper.
import { test as base, expect } from "@playwright/test";
import path from "node:path";
import { fileURLToPath } from "node:url";

export const ASSERT_MS = 30000;
// Drop-in contract: the shop origin comes from BASE_URL and the gauntlet
// origin from GAUNTLET_URL (defaults keep local runs working).
export const SHOP = process.env.BASE_URL || "http://127.0.0.1:18001";
export const GAUNTLET = process.env.GAUNTLET_URL || "http://127.0.0.1:18201";

const here = path.dirname(fileURLToPath(import.meta.url));
export const UPLOAD_FILE = path.join(here, "..", "fixtures", "upload.txt");

export async function acceptCookies(page) {
  await page.goto(`${SHOP}/shop`, { waitUntil: "commit" });
  await page.locator("#cookie-accept").click();
  await expect(page.locator("#cookie")).toBeHidden();
}

export async function signIn(page, email = "demo@acme.test", password = "hunter2") {
  await page.goto(`${SHOP}/login`, { waitUntil: "commit" });
  await page.getByLabel("Email").fill(email);
  await page.getByLabel("Password").fill(password);
  await page.getByRole("button", { name: "Sign in" }).click();
}

export async function gauntletVerdict(page, name) {
  return page.evaluate((n) =>
    fetch(`/verdict/${n}`).then((r) => r.json()).then((j) => j.legitimate), name);
}

export const test = base.extend({
  context: async ({ browser }, use) => {
    const context = await browser.newContext({ acceptDownloads: true });
    await use(context);
    await context.close();
  },
});

export { expect };
