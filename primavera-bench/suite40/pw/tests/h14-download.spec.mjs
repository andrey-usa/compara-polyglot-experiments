import { test, expect, GAUNTLET } from "../fixtures.mjs";
import fs from "node:fs";

test("h14 — downloading the report is a real download", async ({ page }) => {
  await page.goto(`${GAUNTLET}/download`, { waitUntil: "commit" });
  const downloadPromise = page.waitForEvent("download");
  await page.locator("#dl").click();
  const download = await downloadPromise;
  expect(download.suggestedFilename()).toBe("report.csv");
  const body = await fs.promises.readFile(await download.path(), "utf8");
  expect(body).toBe("name,score\nada,100\ngrace,99\n");
});
