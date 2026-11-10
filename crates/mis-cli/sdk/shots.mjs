// Screenshot + smoke harness behind each app's scripts/shots.mjs (`npm run shots`, part of
// `npm run verify`). Run from the app's folder:
//   npm run shots [-- <page url> [out dir] [app args…]]
// No url: serves dist/ with `vite preview` (the listener's CSP) in mock mode, or proxied to
// MIS_APPS_URL when set. Fails on console errors, page errors, CSP violations, failed requests,
// HTTP 4xx/5xx (unless `ok(response)`), requests to another origin and load-error banners.
// Uses Playwright's Chromium, or installed Chrome when that isn't downloaded. The app passes
// `chromium` and `preview` in, as this folder can't resolve the app's node_modules.
import { mkdirSync, readFileSync } from 'node:fs';
import { dirname } from 'node:path';

/** Runs `steps({ open, url, base, mock, out, args })`; `open(scheme)` gives a 390 px page. */
export async function shots({ chromium, preview, ok = () => false }, steps) {
  const slug = JSON.parse(readFileSync('package.json', 'utf8')).name;
  let [url, out = 'shots', ...args] = process.argv.slice(2);
  let server;
  if (!url) {
    server = await preview({ preview: { port: 4173, host: '127.0.0.1' } });
    url = new URL(`/a/${slug}/${process.env.MIS_APPS_URL ? '' : '?mock=1'}`, server.resolvedUrls.local[0]).href;
  }
  const mock = new URL(url).searchParams.get('mock') === '1';
  const base = url.split('#')[0];
  const origin = new URL(url).origin;
  mkdirSync(out, { recursive: true });

  // No browser: stop the preview server too, or the process never exits.
  const browser = await chromium
    .launch()
    .catch(() => chromium.launch({ channel: 'chrome' }))
    .catch(async (e) => {
      await server?.close();
      throw e;
    });
  const problems = [];
  const contexts = [];

  async function open(scheme) {
    const ctx = await browser.newContext({ viewport: { width: 390, height: 844 }, deviceScaleFactor: 2, colorScheme: scheme });
    contexts.push(ctx);
    const page = await ctx.newPage();
    const bad = (what) => problems.push(`${scheme}: ${what}`);
    // Browsers log every 4xx as a console error; judge responses by status instead.
    page.on('console', (m) => m.type() === 'error' && !m.text().startsWith('Failed to load resource') && bad(m.text()));
    page.on('pageerror', (e) => bad(e.message));
    page.on('response', (r) => r.status() >= 400 && !ok(r) && bad(`${r.status()} ${r.url()}`));
    // Aborted: a fetch cut short by navigation or closing the page, not a failure.
    page.on('requestfailed', (r) => r.failure()?.errorText !== 'net::ERR_ABORTED' && bad(`failed ${r.url()}: ${r.failure()?.errorText}`));
    page.on('request', (r) => /^https?:/.test(r.url()) && new URL(r.url()).origin !== origin && bad(`off ${origin}: ${r.url()}`));
    await page.addInitScript(() =>
      document.addEventListener('securitypolicyviolation', (e) => console.error(`CSP: ${e.violatedDirective} ${e.blockedURI}`)),
    );
    const settle = () => page.evaluate(() => new Promise((r) => requestAnimationFrame(() => requestAnimationFrame(r))));
    return {
      ctx,
      page,
      /** `#/<route>` of the page under test, keeping the page (mock data lives in it) when already there. */
      async go(route) {
        await page.evaluate((r) => (location.hash = r), `#/${route}`).catch(() => {});
        if (!page.url().startsWith(base)) await page.goto(`${base}#/${route}`);
        await page.waitForSelector('.screen:not(.empty), .screen.empty:not(:has-text("Loading"))');
        // The apps' load-error banners ("Couldn't load: …"): a caught failure, so no console error.
        const failed = page.getByText(/^Couldn't (load|refresh)/);
        if (await failed.count()) bad(`#/${route}: ${await failed.first().textContent()}`);
      },
      /** `<out>/<name>.png`; full: the whole page, tab bar at its foot (grows the viewport rather
       * than stitch, as fixed bars would repeat); else one 844 px screen. */
      async shot(name, { full = true } = {}) {
        await settle();
        const tall = full ? await page.evaluate(() => document.documentElement.scrollHeight) : 844;
        await page.setViewportSize({ width: 390, height: Math.max(844, tall) });
        await settle();
        const path = `${out}/${name}.png`;
        mkdirSync(dirname(path), { recursive: true });
        await page.screenshot({ path });
        await page.setViewportSize({ width: 390, height: 844 });
      },
    };
  }

  try {
    await steps({ open, url, base, mock, out, args });
  } finally {
    for (const c of contexts) await c.close().catch(() => {});
    await browser.close();
    await server?.close();
    // Also when a step threw: what the page reported first is usually why.
    if (problems.length) console.error(problems.join('\n'));
  }
  if (problems.length) process.exit(1);
  console.log(`screenshots of ${url} in ${out}/`);
}
