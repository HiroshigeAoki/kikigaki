'use strict';
// Dev-only worst-case harness for the settings window (never bundled: crates/kikigaki/ui is the
// Tauri frontendDist, this directory is not).
//
// Builds out/harness.html from the real crates/kikigaki/ui/settings.html with a mocked Tauri
// bridge (mock.js) injected before the page scripts, then screenshots every fixture at the real
// window size (480x640, not resizable) in light and dark, plus a few interaction states, and
// records measured overflow per shot in out/manifest.json.
//
//   node scripts/ui-harness/shoot.js            # build + screenshot everything
//   node scripts/ui-harness/shoot.js --build    # only build out/harness.html for manual viewing
//
// Requires playwright-core (PLAYWRIGHT_CORE=<path to module> if it is not resolvable) and an
// installed Chromium. Open out/harness.html?data=worst in a browser to flip fixtures by hand.
const path = require('path');
const fs = require('fs');

const HERE = __dirname;
const UI = path.resolve(HERE, '../../crates/kikigaki/ui');
const OUT = path.join(HERE, 'out');

function build() {
  fs.mkdirSync(path.join(OUT, 'shots'), { recursive: true });
  const html = fs.readFileSync(path.join(UI, 'settings.html'), 'utf8');
  const firstScript = html.indexOf('<script');
  if (firstScript < 0) throw new Error('settings.html has no <script> tag to anchor the mock');
  const fileUrl = (p) => `file://${p}`;
  const harness = html
    .replace('<head>', `<head>\n    <base href="${fileUrl(UI)}/">`)
    .replace(/<script/, `<script src="${fileUrl(path.join(HERE, 'mock.js'))}"></script>\n    <script`)
    .replace('</body>', `  <script src="${fileUrl(path.join(HERE, 'toggle.js'))}"></script>\n  </body>`);
  const file = path.join(OUT, 'harness.html');
  fs.writeFileSync(file, harness);
  return file;
}

function fixtureNames() {
  const vm = require('vm');
  const sandbox = { window: {}, location: { search: '' }, URLSearchParams: class { get() { return null; } }, structuredClone: (x) => x, setTimeout };
  vm.runInNewContext(fs.readFileSync(path.join(HERE, 'mock.js'), 'utf8'), sandbox);
  return sandbox.window.__HARNESS__.names;
}

const MEASURE = () => {
  const out = [];
  const docW = document.documentElement.clientWidth;
  for (const el of document.querySelectorAll('body *')) {
    if (el.closest('[hidden]') || !el.getClientRects().length) continue;
    const r = el.getBoundingClientRect();
    const id = el.id ? `#${el.id}` : `${el.tagName.toLowerCase()}.${[...el.classList].join('.')}`;
    if (r.right > docW + 0.5) out.push({ el: id, issue: 'past-window-right', right: Math.round(r.right), docW });
    const card = el.parentElement && el.parentElement.closest('section.group, .status, .step');
    if (card) {
      const s = card.getBoundingClientRect();
      if (r.right > s.right + 0.5) out.push({ el: id, issue: 'past-card-right', by: Math.round(r.right - s.right) });
    }
    if (el.scrollWidth > el.clientWidth + 1 && getComputedStyle(el).overflowX !== 'visible') {
      out.push({ el: id, issue: 'clipped-x', scrollWidth: el.scrollWidth, clientWidth: el.clientWidth });
    }
  }
  return {
    docH: document.documentElement.scrollHeight,
    hScroll: document.documentElement.scrollWidth > document.documentElement.clientWidth,
    issues: out.slice(0, 40),
  };
};

async function shoot(harnessFile) {
  const { chromium } = require(process.env.PLAYWRIGHT_CORE || 'playwright-core');
  const url = (q) => `file://${harnessFile}?chrome=0&${q}`;
  const browser = await chromium.launch();
  const manifest = [];
  const names = fixtureNames();
  const shot = async (page, name, fullPage = false) => {
    const file = path.join(OUT, 'shots', `${name}.png`);
    await page.screenshot({ path: file, fullPage });
    manifest.push({ name, file, ...(await page.evaluate(MEASURE)) });
  };
  for (const scheme of ['light', 'dark']) {
    const ctx = await browser.newContext({ viewport: { width: 480, height: 640 }, colorScheme: scheme, locale: 'ja-JP', timezoneId: 'Asia/Tokyo' });
    const page = await ctx.newPage();
    for (const name of names) {
      await page.goto(url(`data=${name}`));
      await page.waitForTimeout(150);
      await shot(page, `${scheme}-${name}`);
      if (scheme === 'light' && (name === 'worst' || name === 'demo')) await shot(page, `${scheme}-${name}-full`, true);
    }
    if (scheme === 'light') {
      await page.goto(url('data=hotkeys-long'));
      await page.waitForTimeout(150);
      await page.click('#longHkButton');
      await page.waitForTimeout(100);
      await shot(page, 'light-capture-long');
      await page.goto(url('data=demo&fail=1'));
      await page.waitForTimeout(150);
      await page.click('#punctSeg button[data-value="off"]');
      await page.click('#transcriptLogSwitch');
      await page.click('#hkButton');
      await page.waitForTimeout(150);
      await shot(page, 'light-command-errors');
      await page.evaluate(() => window.scrollTo(0, 0));
      await page.click('#quitButton');
      await page.waitForTimeout(300);
      await shot(page, 'light-command-errors-bottom');
    }
    await ctx.close();
  }
  await browser.close();
  fs.writeFileSync(path.join(OUT, 'shots', 'manifest.json'), JSON.stringify(manifest, null, 1));
  for (const m of manifest) console.log(`${m.name}: h=${m.docH}${m.hScroll ? ' HSCROLL' : ''} issues=${m.issues.length}`);
}

const harnessFile = build();
console.log(`harness: ${harnessFile}`);
if (!process.argv.includes('--build')) {
  shoot(harnessFile).catch((error) => {
    console.error(error);
    process.exit(1);
  });
}
