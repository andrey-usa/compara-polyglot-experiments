import { test, expect } from '@playwright/test';
test('checkout places the order', async ({ page }) => {
  await page.goto('/shop/p/101');
  await page.getByRole('button', { name: 'Accept all' }).click().catch(() => {});
  await page.locator('#detail-add').click();
  await page.goto('/checkout');
  await page.locator('#name').fill('Ada Lovelace');
  await page.locator('#country').selectOption('United States');
  await page.locator('input[name="speed"][value="standard"]').click();
  await page.locator('#continue').click();
  const frame = page.frameLocator('#payframe');
  await frame.locator('#card').fill('4242424242424242');
  await frame.locator('#exp').fill('12/30');
  await frame.locator('#cvc').fill('123');
  page.on('dialog', d => d.accept());
  await page.locator('#place').click();
  await expect(page.locator('#order-id')).toBeVisible();
});
