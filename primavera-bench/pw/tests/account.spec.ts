import { test, expect } from '@playwright/test';

test('Signing in shows the orders', async ({ page }) => {
  const errors: string[] = [];
  page.on('pageerror', (e) => errors.push(String(e)));
  page.on('console', (m) => { if (m.type() === 'error') errors.push(m.text()); });
  await page.goto('/login');
  await page.getByLabel('Email').fill('demo@acme.test');
  await page.getByLabel('Password').fill(process.env.PASSWORD ?? 'hunter2');
  await page.getByRole('button', { name: 'Sign in' }).click();
  await expect(page).toHaveURL(/\/account$/);
  await expect(page.getByText('Signed in as')).toBeVisible();
  await expect(page.locator('#pageinfo')).toHaveText(/^Page 1 of/);
  expect(errors).toEqual([]);
});

test('A wrong password stays on the sign-in page', async ({ page }) => {
  await page.goto('/login');
  await page.getByLabel('Email').fill('demo@acme.test');
  await page.getByLabel('Password').fill('wrong');
  await page.getByRole('button', { name: 'Sign in' }).click();
  await expect(page.getByRole('alert')).toContainText('Wrong email or password');
  await expect(page).toHaveTitle(/Sign in/);
});

test('The returns FAQ opens', async ({ page }) => {
  await page.goto('/docs');
  await page.getByText('Returns FAQ').click();
  await expect(page.getByText('days from delivery')).toBeVisible();
  await expect(page.locator('#returns-faq')).toHaveAttribute('open', '');
});
