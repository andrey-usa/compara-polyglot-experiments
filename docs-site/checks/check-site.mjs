// Site checks for the T-0083 Starlight build (CI + local).
// Usage: node check-site.mjs <dist-dir> [--port N]
//  - axe: every HTML page in dist, via @axe-core/playwright on Chromium.
//    Fails on any critical or serious violation (the T-0083 bar).
//  - pagefind: 10 queries against the built _pagefind index (the same
//    index Starlight's search UI uses), loaded in the browser. Each
//    query passes when one of its expected pages is in the top 5.
// Writes axe-results.json and search-results.json next to this script.
// Exit non-zero when any check fails.
import { spawn } from 'node:child_process';
import { readdirSync, statSync, writeFileSync } from 'node:fs';
import { join, relative } from 'node:path';
import { chromium } from 'playwright';
import AxeBuilder from '@axe-core/playwright';

const dist = process.argv[2];
const portArg = process.argv.indexOf('--port');
const port = portArg >= 0 ? process.argv[portArg + 1] : '8931';
if (!dist) { console.error('usage: node check-site.mjs <dist-dir>'); process.exit(2); }

const QUERIES = [
  { q: 'install navigera', expect: ['/navigera/install/'] },
  { q: 'compare two tables', expect: ['/compara/', '/compara/quickstart/'] },
  { q: 'primavera scenario', expect: ['/primavera/guides/', '/primavera/'] },
  { q: 'multi-model', expect: ['/navigera/multi-model/'] },
  { q: 'agent skill', expect: ['/navigera/skill/', '/primavera/skill/'] },
  { q: 'changelog', expect: ['/primavera/changelog/', '/navigera/changelog/', '/compara/changelog/'] },
  { q: 'docker', expect: ['/compara/install/', '/primavera/install/'] },
  { q: 'screenshot', expect: ['/navigera/guides/', '/navigera/cli-reference/'] },
  { q: 'duplicate keys', expect: ['/compara/', '/compara/quickstart/'] },
  { q: 'headless', expect: ['/navigera/quickstart/', '/navigera/cli-reference/'] },
];

function pages(dir, base = dir) {
  let out = [];
  for (const e of readdirSync(dir, { withFileTypes: true })) {
    const p = join(dir, e.name);
    if (e.isDirectory()) out = out.concat(pages(p, base));
    else if (e.name.endsWith('.html')) out.push(relative(base, p));
  }
  return out.sort();
}

const server = spawn('python3', ['-m', 'http.server', port, '--directory', dist], { stdio: 'ignore' });
await new Promise(r => setTimeout(r, 1500));
const base = `http://127.0.0.1:${port}`;
let failed = false;
try {
  const browser = await chromium.launch(process.env.CHROME_EXECUTABLE_PATH
    ? { executablePath: process.env.CHROME_EXECUTABLE_PATH, args: ['--no-sandbox'] }
    : { args: ['--no-sandbox'] });
  const page = await (await browser.newContext()).newPage();
  // --- axe, every page ---
  const axeResults = [];
  for (const rel of pages(dist)) {
    const url = `${base}/${rel}`;
    await page.goto(url);
    const results = await new AxeBuilder({ page }).analyze();
    const byImpact = {};
    for (const v of results.violations) byImpact[v.impact || 'none'] = (byImpact[v.impact || 'none'] || 0) + 1;
    const bad = (byImpact.critical || 0) + (byImpact.serious || 0);
    if (bad) failed = true;
    axeResults.push({ page: '/' + rel.replace(/index\.html$/, ''), violations: results.violations.length, byImpact,
      rules: results.violations.map(v => `${v.id} (${v.impact}): ${v.nodes.length} nodes`),
      nodes: results.violations.flatMap(v => v.nodes.map(n => ({ rule: v.id, target: n.target, html: n.html.slice(0, 160) }))) });
    console.log(`axe ${'/' + rel.replace(/index\.html$/, '')}: ${results.violations.length} violations ${JSON.stringify(byImpact)}`);
  }
  writeFileSync(new URL('./axe-results.json', import.meta.url), JSON.stringify(axeResults, null, 2));
  // --- pagefind, 10 queries, through Starlight's real search UI ---
  const searchResults = [];
  for (const { q, expect } of QUERIES) {
    await page.goto(`${base}/`);
    await page.locator('button[data-open-modal]').click();
    const input = page.locator('dialog input.pagefind-ui__search-input').first();
    await input.waitFor({ timeout: 10000 });
    await input.fill(q);
    await page.waitForTimeout(1200);
    const hits = await page.locator('.pagefind-ui__results a, dialog a[href]').evaluateAll(
      els => [...new Set(els.map(a => a.getAttribute('href')))].slice(0, 5));
    await page.keyboard.press('Escape');
    const rank = hits.findIndex(u => expect.some(e => u.includes(e)));
    const ok = rank >= 0;
    if (!ok) failed = true;
    searchResults.push({ query: q, top3: hits, expected: expect, rank: rank >= 0 ? rank + 1 : null, ok });
    console.log(`search ${JSON.stringify(q)}: ${ok ? 'ok rank ' + (rank + 1) : 'FAIL'} top3=${JSON.stringify(hits)}`);
  }
  writeFileSync(new URL('./search-results.json', import.meta.url), JSON.stringify(searchResults, null, 2));
  await browser.close();
} finally {
  server.kill();
}
console.log(failed ? 'CHECKS FAILED' : 'ALL CHECKS PASSED');
process.exit(failed ? 1 : 0);
