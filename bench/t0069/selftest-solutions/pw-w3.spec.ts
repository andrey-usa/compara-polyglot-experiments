import { test, expect } from '@playwright/test';
test('adding a product from the grid counts it in the cart', async ({ page }) => {
  await page.goto('/shop');
  await page.getByRole('button', { name: 'Accept all' }).click().catch(() => {});
  await page.getByRole('button', { name: 'Add Nickel Hinge to cart' }).click();
  await expect(page.locator('#cart-count')).toHaveText('1');
});
