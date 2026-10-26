import { test, expect } from '@playwright/test';
test('shadow search still narrows', async ({ page }) => {
  await page.goto('/shop');
  await page.getByRole('button', { name: 'Accept all' }).click().catch(() => {});
  await page.locator('#q').fill('copper hinge');
  await page.locator('#q').press('Enter');
  await expect(page.getByRole('status')).toContainText('1 product');
  await expect(page.getByRole('link', { name: 'Copper Hinge' })).toBeVisible();
});
