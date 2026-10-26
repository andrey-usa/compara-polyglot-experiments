---
name: playwright-test
description: Write and run web tests with Playwright Test (TypeScript) on real Chromium — locate elements by role, label or text, act, and assert with auto-retrying expect. Use to check a web app after a change, to turn manual exploration into a regression test, or to automate a browser job.
---

# Playwright Test

`@playwright/test` runs `.spec.ts` files on Chromium. Each test gets a fresh
isolated `page`. Actions auto-wait for the element to be actionable; web
assertions auto-retry until they hold, so tests stay non-flaky without
manual sleeps.

## A file

```ts
import { test, expect } from '@playwright/test';

test.beforeEach(async ({ page }) => {
  await page.goto('/shop');
  const banner = page.getByRole('button', { name: 'Accept' });
  if (await banner.isVisible()) await banner.click();
});

test('adding a product counts it in the cart', async ({ page }) => {
  await page.getByRole('button', { name: 'Add to cart' }).first().click();
  await expect(page.getByTestId('cart-count')).toHaveText('1');
  await page.goto('/cart');
  await expect(page.getByText('Your cart')).toBeVisible();
});
```

- Top level: `test.describe` groups, `test.beforeEach`/`afterEach` hooks,
  `test('name', async ({ page }) => { ... })`. Tags: `test('x', { tag: '@smoke' }, ...)`.
- Configure once in `playwright.config.ts`: `use: { baseURL: 'http://127.0.0.1:8000' }`,
  then `page.goto('/cart')` uses relative URLs.

## Locators (find elements; always preferred over CSS/XPath)

```ts
page.getByRole('button', { name: 'Pay' })        // role + accessible name
page.getByLabel('Email')                          // form control by label
page.getByText('Sign in')                         // visible text
page.getByTestId('total')                         // data-testid attribute
page.getByPlaceholder('Search')                   // input placeholder
page.getByRole('button', { name: 'Add' }).nth(1)  // disambiguate; .first() .last()
page.frameLocator('iframe').getByLabel('Card')    // inside iframes
```

Locators are lazy: they re-resolve the element on every action/assertion.
Strict mode fails a locator that matches more than one element — make it
more specific or use `.nth()`. Open shadow roots are pierced automatically.

## Actions (auto-wait for actionable first)

| Action | Use |
| --- | --- |
| `locator.click()` | click |
| `locator.fill('text')` | clear + type into a field |
| `locator.press('Enter')` | single key |
| `locator.check()` / `uncheck()` | checkbox |
| `locator.selectOption('x')` | dropdown option |
| `locator.hover()` | hover |
| `locator.setInputFiles('f.txt')` | file upload |
| `page.goto('/path')` | navigate (waits for load) |
| `page.waitForURL('**/cart')` | wait for SPA navigation |

## expect (auto-retries until it holds, 5 s default)

```ts
await expect(page).toHaveURL(/cart/);        // toHaveTitle(/Check/)
await expect(locator).toBeVisible();         // toBeHidden toBeEnabled toBeChecked
await expect(locator).toHaveText('1');       // exact; toContainText for substring
await expect(locator).toHaveCount(3);        // list length
await expect(locator).toHaveValue('a@b.c');  // input value
await expect(response).toBeOK();             // HTTP 200s
```

Always `await` assertions. Non-retrying matchers (`toBe`, `toEqual`) are for
plain values, not page state — prefer the async ones above or tests go flaky.

## Run

```bash
npm init playwright@latest        # or: npm i -D @playwright/test && npx playwright install chromium
npx playwright test               # all tests, headless
npx playwright test cart.spec.ts  # one file
npx playwright test --grep "cart" # by name
npx playwright test --headed      # watch it
npx playwright show-report        # HTML report of the last run
```

Each test runs in isolation (fresh browser context): no cookies or state
leak between tests. Sign in once per test through the UI, or reuse saved
storage state via `use: { storageState: 'auth.json' }`.

## New tabs, dialogs, downloads

```ts
const popup = await Promise.all([
  page.waitForEvent('popup'),          // target=_blank opens a tab
  page.getByRole('link', { name: 'Docs' }).click(),
]).then(([p]) => p);
await expect(popup).toHaveTitle(/Docs/);

page.on('dialog', d => d.accept());    // confirm()/alert(): accept or dismiss
const download = await Promise.all([
  page.waitForEvent('download'),
  page.getByRole('link', { name: 'Export' }).click(),
]).then(([d]) => d);
await download.saveAs('out.csv');
```

## Waiting on the app (not on timers)

```ts
await page.waitForResponse(r => r.url().includes('/api/products') && r.ok());
await expect(page.getByRole('listitem')).toHaveCount(20);  // lazy list grew
await page.getByRole('button', { name: 'Save' }).waitFor(); // element appears
```

Never `waitForTimeout` for app state — wait for a response, URL, or locator.

## Reading the page

```ts
const total = await page.getByTestId('total').innerText();
const n = await page.getByRole('listitem').count();
const html = await page.locator('#state').textContent();
const data = await page.evaluate(() => JSON.parse(document.body.dataset.state!));
```

## Config essentials (playwright.config.ts)

```ts
export default defineConfig({
  timeout: 30_000,                 // per test
  expect: { timeout: 5_000 },      // per assertion
  retries: process.env.CI ? 2 : 0,
  workers: 4,                      // tests run in parallel workers
  use: { baseURL: 'http://127.0.0.1:8000', browserName: 'chromium',
         trace: 'retain-on-failure', screenshot: 'only-on-failure' },
});
```

## Debugging a failure

- `npx playwright test --debug` steps through with the inspector.
- `npx playwright show-trace test-results/.../trace.zip` replays the run:
  every action, DOM snapshot, console and network.
- The HTML report (`show-report`) lists failures first with screenshots.

## Notes

- One `browser` per worker process; tests share nothing. Put shared setup
  (seed data, auth) in `beforeEach` or storage state, never in globals.
- `page.on('dialog')` must be registered before the action that opens it.
- Locator strategies ranked: role/label > text/testid > placeholder > CSS.
