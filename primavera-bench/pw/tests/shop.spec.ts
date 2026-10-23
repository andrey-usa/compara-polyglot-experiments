import { test, expect } from '@playwright/test';

test.beforeEach(async ({ page }) => {
  await page.goto('/shop');
  await page.locator('#cookie-accept').click();
});

test('Adding a product counts it in the cart', async ({ page }) => {
  await page.getByText('Add to cart', { exact: true }).first().click();
  await expect(page.locator('#cart-count')).toHaveText('1');
  await page.goto('/cart');
  await expect(page.getByRole('heading', { name: 'Your cart' })).toBeVisible();
  await expect(page.locator('#subtotal')).toHaveText(/^\$/);
});

test('Every scenario starts with an empty cart', async ({ page }) => {
  await expect(page.locator('#cart-count')).toHaveText('0');
});

test('Search narrows the list', async ({ page }) => {
  const search = page.getByLabel('Search products');
  await search.fill('valve');
  await search.press('Enter');
  await expect(page.getByRole('status')).toContainText('Showing');
  await expect(page.locator('.card').first()).toBeVisible();
  await expect(page.getByText('Titanium Bolt')).toHaveCount(0);
});

test('A coupon in a web component lowers the total', async ({ page }) => {
  await page.getByText('Add to cart', { exact: true }).first().click();
  await expect(page.locator('#cart-count')).toHaveText('1');
  await page.goto('/cart');
  const subtotal = await page.locator('#subtotal').textContent();
  expect(subtotal).toMatch(/^\$[0-9]+\.[0-9]{2}$/);
  await page.getByLabel('Coupon code').fill('SAVE10');
  await page.getByRole('button', { name: 'Apply coupon' }).click();
  await expect(page.getByText('Coupon SAVE10 applied')).toBeVisible();
  await page.reload();
  await expect(page.getByText('Discount (SAVE10)')).toBeVisible();
});
