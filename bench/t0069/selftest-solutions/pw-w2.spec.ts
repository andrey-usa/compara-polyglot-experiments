import { test, expect } from '@playwright/test';
test('price low to high lists products cheapest first', async ({ page }) => {
  await page.goto('/shop');
  await page.getByRole('button', { name: 'Accept all' }).click().catch(() => {});
  await page.getByLabel('Sort by').selectOption({ label: 'Price: low to high' });
  await expect(page.getByRole('status')).toHaveText(/of 60 products/);
  await expect(page.locator('#grid .card h3').first()).toHaveText('Nickel Spring');
  await expect(page.locator('#grid .card .price').first()).toHaveText('$3.04');
});
