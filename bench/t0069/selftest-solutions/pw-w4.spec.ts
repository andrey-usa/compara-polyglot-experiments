import { test, expect } from '@playwright/test';
test('cart lists line items and a consistent subtotal', async ({ page }) => {
  await page.goto('/shop');
  await page.getByRole('button', { name: 'Accept all' }).click().catch(() => {});
  await page.getByRole('button', { name: 'Add Nickel Hinge to cart' }).click();
  await page.getByRole('button', { name: 'Add Nickel Widget to cart' }).click();
  await page.goto('/cart');
  await expect(page.getByRole('cell', { name: 'Nickel Hinge', exact: true })).toBeVisible();
  await expect(page.getByRole('cell', { name: 'Nickel Widget', exact: true })).toBeVisible();
  await expect(page.locator('#subtotal')).toHaveText('$49.55');
});
