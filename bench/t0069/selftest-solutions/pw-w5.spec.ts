import { test, expect } from '@playwright/test';
test('a valid coupon shows a discount and lowers the total', async ({ page }) => {
  await page.goto('/shop');
  await page.getByRole('button', { name: 'Accept all' }).click().catch(() => {});
  await page.getByRole('button', { name: 'Add Nickel Hinge to cart' }).click();
  await page.goto('/cart');
  await page.getByLabel('Coupon code').fill('SAVE10');
  await page.getByRole('button', { name: 'Apply coupon' }).click();
  await expect(page.getByText('Discount (SAVE10): −$3.42')).toBeVisible();
  await page.goto('/checkout');
  await expect(page.getByText('Order total before shipping: $30.77')).toBeVisible();
});
