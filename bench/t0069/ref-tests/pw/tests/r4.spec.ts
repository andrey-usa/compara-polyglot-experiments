import { test, expect } from '@playwright/test';
test('search narrows the list', async ({ page }) => {
  await page.goto('/shop');
  await page.getByRole('button', { name: 'Accept all' }).click().catch(() => {});
  await page.getByLabel('Search products').fill('hinge');
  await page.getByLabel('Search products').press('Enter');
  await expect(page.getByRole('status')).toContainText('products');
  await expect(page.getByRole('link', { name: 'Nickel Hinge' })).toBeVisible();
});
