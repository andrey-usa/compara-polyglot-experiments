import { test, expect } from '@playwright/test';
test('search narrows the product list', async ({ page }) => {
  await page.goto('/shop');
  await page.getByRole('button', { name: 'Accept all' }).click().catch(() => {});
  await page.getByLabel('Search products').fill('Hinge');
  await page.getByLabel('Search products').press('Enter');
  await expect(page.getByRole('status')).toHaveText('Showing 6 of 6 products');
  await expect(page.getByRole('link', { name: 'Nickel Hinge' })).toBeVisible();
});
