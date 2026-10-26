import { test, expect } from '@playwright/test';
test('renamed button still adds', async ({ page }) => {
  await page.goto('/shop');
  await page.getByRole('button', { name: 'Accept all' }).click().catch(() => {});
  await page.getByRole('button', { name: 'Add Nickel Hinge to basket' }).click();
  await expect(page.locator('#cart-count')).toHaveText('1');
});
