// Playwright's dragTo of Gift onto Internet on Actual's mobile budget (as
// upstream's test does it, and at the row center), with the page's drag
// event log. Uses the playwright package Actual's own install brings.
import { createRequire } from 'node:module';
import { readFileSync } from 'node:fs';
const require = createRequire(process.env.ACTUAL_DIR + '/packages/desktop-client/package.json');
const { chromium } = require('@playwright/test');
const logger = readFileSync(new URL('./logger.js', import.meta.url), 'utf8');
const browser = await chromium.launch({ executablePath: process.env.CHROME_BIN || undefined });
for (const [label, opts] of [['pw-top-edge', { targetPosition: { x: 20, y: 5 } }], ['pw-center', {}]]) {
  const context = await browser.newContext({ userAgent: 'playwright' });
  const page = await context.newPage();
  await page.setViewportSize({ width: 350, height: 600 });
  await page.goto('http://127.0.0.1:3001/');
  await page.getByRole('button', { name: 'Try the demo' }).click();
  await page.getByTestId('budget-table').waitFor({ timeout: 60000 });
  await page.setViewportSize({ width: 350, height: 1000 });
  await page.evaluate(() => [...document.querySelectorAll('[data-navbar-state] a')].find((a) => a.textContent.trim() === 'Budget').click());
  await page.getByTestId('budget-table').waitFor({ timeout: 60000 });
  await page.evaluate(logger);
  const rows = page.getByTestId('budget-table').getByTestId('category-row');
  const gift = rows.filter({ hasText: 'Gift' }), internet = rows.filter({ hasText: 'Internet' });
  console.log(`${label} before: ${await page.evaluate(() => window.__rows('Bills'))}`);
  await gift.dragTo(internet, opts);
  await page.waitForTimeout(2000);
  console.log(`${label} dump: ${await page.evaluate(() => window.__dump())}`);
  await context.close();
}
await browser.close();
