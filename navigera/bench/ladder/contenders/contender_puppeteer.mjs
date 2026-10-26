#!/usr/bin/env node
// Contender: Puppeteer (puppeteer-core, executablePath = system chrome).
//
// Same canonical session / micro modes as contender_playwright.mjs; see it
// for the mode contract. The harness measures this process with wait4.

import { writeFileSync } from 'node:fs';
import puppeteer from 'puppeteer-core';

const EVAL_TITLE = `(() => document.title)()`;
const EVAL_CARD_COUNT = `(() => document.querySelectorAll('.card').length)()`;
const EVAL_EXTRACT = `(() => Array.from(document.querySelectorAll('.card')).map(c => ({
  t: c.querySelector('.t').textContent,
  p: c.querySelector('.p').textContent,
  v: c.dataset.vendor
})))()`;
const EVAL_VISIBLE = `(() => document.querySelectorAll('.card:not(.hidden)').length)()`;
const EVAL_ROWS = `(() => document.querySelectorAll('#rows tr').length)()`;

const mode = process.argv[2] || 'session';
const base = process.env.LADDER_BASE;
const chromeBin = process.env.CHROME_BIN;
const nEval = Number(process.env.LADDER_N_EVAL || '200');

async function launch() {
  const browser = await puppeteer.launch({
    executablePath: chromeBin,
    headless: 'new',
    args: [
      '--disable-blink-features=AutomationControlled',
      '--no-first-run',
      '--no-default-browser-check',
      '--disable-infobars',
      '--window-size=1440,900',
      '--no-sandbox',
      '--disable-dev-shm-usage',
    ],
  });
  const page = (await browser.pages())[0] || (await browser.newPage());
  await page.setViewport({ width: 1440, height: 900 });
  return { browser, page };
}

async function session() {
  const counts = {};
  const { browser, page } = await launch();
  await page.goto(`${base}/page_a.html`, { waitUntil: 'load' });
  counts.title_a = await page.evaluate(EVAL_TITLE);
  counts.cards_a = await page.evaluate(EVAL_CARD_COUNT);
  const cards = await page.evaluate(EVAL_EXTRACT);
  writeFileSync(process.env.LADDER_EXTRACT_OUT, JSON.stringify(cards));
  await page.type('#q', 'widget');
  await page.click('#search');
  counts.visible = await page.evaluate(EVAL_VISIBLE);
  await page.screenshot({ path: process.env.LADDER_SHOT_OUT });
  const page2 = await browser.newPage();
  await page2.setViewport({ width: 1440, height: 900 });
  await page2.goto(`${base}/page_b.html`, { waitUntil: 'load' });
  counts.title_b = await page2.evaluate(EVAL_TITLE);
  counts.rows_b = await page2.evaluate(EVAL_ROWS);
  await browser.close();
  return counts;
}

async function evalMicro() {
  const { browser, page } = await launch();
  await page.goto(`${base}/page_a.html`, { waitUntil: 'load' });
  const lat = [];
  for (let i = 0; i < nEval; i++) {
    const t0 = performance.now();
    await page.evaluate(EVAL_TITLE);
    lat.push(performance.now() - t0);
  }
  await browser.close();
  return lat;
}

async function cold() {
  const { browser, page } = await launch();
  await page.evaluate(`(() => 1 + 1)()`);
  await browser.close();
}

async function realworld() {
  const { browser, page } = await launch();
  const ops = {};
  let t = performance.now();
  await page.goto('https://example.com', { waitUntil: 'load' });
  ops.goto_ms = performance.now() - t;
  t = performance.now();
  const title = await page.evaluate(`(() => document.title)()`);
  ops.title_ms = performance.now() - t;
  const h1 = await page.evaluate(`(() => document.querySelector('h1')?.textContent ?? null)()`);
  const para = await page.evaluate(`(() => document.querySelector('p')?.textContent?.trim().slice(0, 80) ?? null)()`);
  await browser.close();
  return { title, h1, para, ops };
}

async function browse() {
  const { browser, page } = await launch();
  const facts = {};
  const ops = {};
  let t = performance.now();
  await page.goto('https://github.com/sindresorhus/awesome', { waitUntil: 'load' });
  ops.awesome_goto_ms = performance.now() - t;
  facts.awesome_title = await page.evaluate(`(() => document.title ?? null)()`);
  facts.awesome_links = await page.evaluate(`(() => document.querySelectorAll('a').length)()`);
  const scrollYs = [];
  for (let i = 0; i < 5; i++) {
    t = performance.now();
    scrollYs.push(await page.evaluate(`(() => { window.scrollBy({ top: 800, behavior: 'instant' }); return window.scrollY; })()`));
    ops[`awesome_scroll${i}_ms`] = performance.now() - t;
    await new Promise(r => setTimeout(r, 300));
  }
  facts.awesome_scroll_ys = scrollYs;
  facts.awesome_headings = await page.evaluate(`(() => [...document.querySelectorAll('h2')].slice(0, 5).map(h => h.textContent?.trim() ?? null))()`);
  t = performance.now();
  await page.goto('https://github.com/trending', { waitUntil: 'load' });
  ops.trending_goto_ms = performance.now() - t;
  facts.trending_title = await page.evaluate(`(() => document.title ?? null)()`);
  facts.trending_repos = await page.evaluate(`(() => document.querySelectorAll('article.Box-row').length)()`);
  facts.trending_top3 = await page.evaluate(`(() => [...document.querySelectorAll('article.Box-row h2 a')].slice(0, 3).map(a => (a.textContent ?? '').replace(/\\s+/g, '')))()`);
  t = performance.now();
  await page.click('article.Box-row h2 a');
  let newUrl = null;
  for (let i = 0; i < 24; i++) {
    await new Promise(r => setTimeout(r, 250));
    newUrl = page.url();
    if (newUrl && !newUrl.includes('trending')) break;
  }
  ops.trending_click_ms = performance.now() - t;
  facts.trending_clicked_url = newUrl;
  facts.trending_clicked_title = await page.evaluate(`(() => document.title ?? null)()`);
  await browser.close();
  return { facts, ops };
}

const t0 = performance.now();
try {
  if (mode === 'session') {
    const counts = await session();
    console.log(JSON.stringify({ mode, wall_ms: performance.now() - t0, counts }));
  } else if (mode === 'eval') {
    const lat = await evalMicro();
    console.log(JSON.stringify({ mode, wall_ms: performance.now() - t0 }));
    console.log(JSON.stringify({ eval_ms: lat }));
  } else if (mode === 'cold') {
    await cold();
    console.log(JSON.stringify({ mode, wall_ms: performance.now() - t0 }));
  } else if (mode === 'realworld') {
    const rw = await realworld();
    console.log(JSON.stringify({ mode, wall_ms: performance.now() - t0, realworld: rw }));
  } else if (mode === 'browse') {
    const br = await browse();
    console.log(JSON.stringify({ mode, wall_ms: performance.now() - t0, realworld: br }));
  } else {
    throw new Error(`unknown mode ${mode}`);
  }
} catch (e) {
  console.error(`[puppeteer] fatal: ${e && e.stack ? e.stack : e}`);
  process.exit(1);
}
