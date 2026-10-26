import { test, expect } from '@playwright/test';
import * as fs from 'fs';
test('support request with an attachment confirms a ticket id', async ({ page }) => {
  fs.writeFileSync('w7-note.txt', 'delivery photo\n');
  await page.goto('/support');
  await page.getByRole('button', { name: 'Accept all' }).click().catch(() => {});
  const widget = page.frameLocator('iframe[title="Support request form"]');
  await widget.getByLabel('Category').selectOption({ label: 'Orders' });
  await widget.getByLabel('Message').fill('Order A-1042 never arrived.');
  await widget.getByLabel('Attachment (optional)').setInputFiles('w7-note.txt');
  await widget.getByRole('button', { name: 'Submit request' }).click();
  await expect(widget.getByText('Your ticket T-501 was created.')).toBeVisible();
});
