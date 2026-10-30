// Browser checks for the resampling filter control on the Displays tab.
// Run from the repository root; see README.md in this directory. The fixture
// intercepts every request to suede.test and never connects to an appliance.
const assert = require('node:assert/strict');
const fs = require('node:fs');
const { chromium } = require('playwright');

const html = fs.readFileSync('src/api/ui/index.html', 'utf8');
const fixture = JSON.parse(fs.readFileSync('docs/examples/four-output-warp.json', 'utf8'));
for (const output of fixture.outputs ?? []) {
  if (output.geometry && Object.hasOwn(output.geometry, 'source')) {
    output.geometry.slice = output.geometry.source;
    delete output.geometry.source;
  }
}
// The daemon omits `filter` while it is the default; mirror that.
delete fixture.projection.filter;

const interval = {
  canvasFps: 60, presentedFps: 60, freeRun: false, renderer: 'gpu',
  captureIntervals: { one: 60, two: 0, three: 0, more: 0 }, offsetMs: null, straddles: 0,
  gateHolds: 0, framesSuperseded: 0, stalls: 0,
  perFrameMs: { waiting: 1, snapshot: 1, requesting: 1, blending: 1, gpu: 0 }, measuredAt: 1,
  outputs: fixture.outputs.filter(o => o.enable !== false).map(o => ({
    name: o.match.name, presented: 60, discarded: 0, zeroCopyPresented: 0, refreshHz: 60,
    phaseMs: null, phaseSpreadMs: null, lagFrames: { zero: 0, one: 0, two: 0, more: 0 }, commitToPresentMs: null,
  })),
};

(async () => {
  const browser = await chromium.launch({ headless: true });
  const failures = [];
  const puts = [];
  try {
    const context = await browser.newContext({ viewport: { width: 1280, height: 900 }, colorScheme: 'light' });
    const page = await context.newPage();
    page.on('pageerror', error => failures.push(error.message));
    await page.addInitScript(() => localStorage.setItem('suede.feeNoticeAcknowledged', '1'));
    let current = structuredClone(fixture);
    const samplingNames = interval.outputs.map(o => o.name);
    await page.route('**/*', async route => {
      const request = route.request();
      const url = new URL(request.url());
      const respond = (body, headers = {}) => route.fulfill({ status: 200, contentType: 'application/json',
        headers, body: JSON.stringify(body) });
      if (url.pathname === '/') return route.fulfill({ contentType: 'text/html', body: html });
      if (url.pathname === '/assets/fonts/fonts.css') return route.fulfill({
        contentType: 'text/css', body: fs.readFileSync('fonts/fonts.css') });
      if (url.pathname.startsWith('/assets/fonts/') && url.pathname.endsWith('.woff2')) return route.fulfill({
        contentType: 'font/woff2', body: fs.readFileSync(`fonts/${url.pathname.slice('/assets/fonts/'.length)}`) });
      const path = url.pathname.replace('/api/v1', '');
      const headers = { 'x-config-generation': '0', 'x-config-epoch': 'filter-fixture', etag: '"0"' };
      if (path === '/config' && request.method() === 'PUT') {
        const body = request.postDataJSON();
        puts.push(body);
        current = structuredClone(body);
        return respond(current, headers);
      }
      if (path === '/config') return respond(current, headers);
      if (path === '/projection/stats') return respond({
        running: true, lastInterval: interval,
        geometry: { warpAvailable: true, effectiveMode: current.projection.mode },
        control: { session: 's', outputs: [
          { name: samplingNames[0], samplingMode: 'lanczos' },
          { name: samplingNames[1], samplingMode: 'bilinear', samplingNote: 'easu is not implemented; sampled bilinear' },
        ] },
      });
      if (path === '/av') return respond({ audioOutputs: [], audioInputs: [], videoInputs: [] });
      if (path === '/system') return respond({ suedeVersion: 'test', uptimeSeconds: 1, packages: [] });
      if (path === '/status') return respond({ state: 'synced', divergences: [] });
      if (path === '/system/gpu') return respond({ available: true, gpus: [], processes: [] });
      if (path === '/system/cpu') return respond({});
      if (path === '/system/pipeline') return respond(null);
      return respond([]);
    });

    await page.goto('http://suede.test/?nosse');
    await page.waitForFunction(() => config && configGeneration === '0');

    // A document without `filter` shows bilinear.
    assert.equal(await page.locator('#pj-filter').inputValue(), 'bilinear');
    assert.deepEqual(await page.locator('#pj-filter option').evaluateAll(o => o.map(x => x.value)),
      ['bilinear', 'nearest', 'bicubic', 'lanczos', 'easu']);
    assert(await page.locator('#pj-filter').isVisible());
    assert.match(await page.locator('#pj-filter-hint').textContent(), /does not map 1:1/);

    // Choosing a filter previews a document that carries it.
    await page.locator('#pj-filter').selectOption('lanczos');
    await page.evaluate(() => flushPreviews());
    assert.equal(puts.at(-1).projection.filter, 'lanczos');
    assert.equal(puts.at(-1).committed, false);

    // Returning to the default drops the field, as the daemon does.
    await page.locator('#pj-filter').selectOption('bilinear');
    await page.evaluate(() => flushPreviews());
    assert(!Object.hasOwn(puts.at(-1).projection, 'filter'), 'bilinear is omitted');

    // Telemetry shows what each output actually samples with, and the note.
    await page.evaluate(async () => { projectionStats = await api('/projection/stats'); renderProjectionTiming(); });
    const sampling = await page.locator('#pj-timing-outputs tr').evaluateAll(rows =>
      rows.map(r => r.lastElementChild.textContent));
    assert.match(sampling[0], /^lanczos$/);
    assert.match(sampling[1], /^bilinear\s*easu is not implemented; sampled bilinear$/);
    assert.equal(sampling[2], '—', 'an output without a report shows a dash');

    // Exact mode is strictly 1:1, so the control is hidden and the value kept.
    await page.evaluate(() => { config.projection.mode = 'exact'; config.projection.filter = 'nearest'; renderProjection(); });
    assert(!(await page.locator('#pj-filter').isVisible()), 'hidden in Exact mode');
    assert(!(await page.locator('#pj-filter-hint').isVisible()));
    assert.equal(await page.evaluate(() => config.projection.filter), 'nearest', 'the saved value is untouched');
    await page.evaluate(() => { config.projection.mode = 'simple'; renderProjection(); });
    assert(await page.locator('#pj-filter').isVisible());
    assert.equal(await page.locator('#pj-filter').inputValue(), 'nearest');
  } catch (error) {
    failures.push(error.stack || String(error));
  } finally {
    await browser.close();
  }
  if (failures.length) { console.error(failures.join('\n')); process.exit(1); }
  console.log('filter UI checks passed');
})();
