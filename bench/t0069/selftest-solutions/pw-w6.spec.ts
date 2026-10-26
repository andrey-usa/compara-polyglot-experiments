import { test, expect } from '@playwright/test';
test('signing in shows the order history', async ({ page }) => {
  await page.goto('/login');
  await page.getByRole('button', { name: 'Accept all' }).click().catch(() => {});
  await page.getByLabel('Email').fill('demo@acme.test');
  await page.getByLabel('Password').fill('hunter2');
  await page.getByRole('button', { name: 'Sign in' }).click();
  await expect(page.getByRole('heading', { name: 'Your orders' })).toBeVisible();
  await expect(page.getByText('Signed in as demo@acme.test')).toBeVisible();
  await expect(page.getByRole('cell', { name: 'A-1060' })).toBeVisible();
});
