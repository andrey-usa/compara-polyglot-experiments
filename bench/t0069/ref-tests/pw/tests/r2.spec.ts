import { test, expect } from '@playwright/test';
test('open the product detail', async ({ page }) => {
  await page.goto('/shop');
  await page.getByRole('button', { name: 'Accept all' }).click().catch(() => {});
  await page.getByRole('link', { name: 'Nickel Hinge' }).click();
  await expect(page.getByRole('heading', { name: 'Nickel Hinge' })).toBeVisible();
  await expect(page.locator('#detail-add')).toBeVisible();
});
