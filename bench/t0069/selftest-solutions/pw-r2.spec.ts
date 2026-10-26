import { test, expect } from '@playwright/test';
test('card button opens detail', async ({ page }) => {
  await page.goto('/shop');
  await page.getByRole('button', { name: 'Accept all' }).click().catch(() => {});
  await page.locator('.card-open').first().click();
  await expect(page.getByRole('heading', { name: 'Nickel Hinge' })).toBeVisible();
  await expect(page.locator('#detail-add')).toBeVisible();
});
