// Real Chromium checks for the consolidated Displays editor.
const { chromium } = require('playwright');
const fs = require('node:fs');
const assert = require('node:assert/strict');
const fixture = JSON.parse(fs.readFileSync('docs/examples/four-output-warp.json'));
const html = fs.readFileSync('src/api/ui/index.html', 'utf8');
const sleep = ms => new Promise(resolve => setTimeout(resolve, ms));

(async () => {
  const browser = await chromium.launch({ headless: true });
  let current = structuredClone(fixture), committed = structuredClone(fixture), generation = 0;
  let recommendationCalls = 0, recommendationDelay = 0, inFlight = 0, maxFlight = 0;
  const context = await browser.newContext({ viewport: { width: 1280, height: 1000 } });
  await context.route('**/*', async route => {
    const request = route.request(), url = new URL(request.url()), path = url.pathname.replace('/api/v1', '');
    const reply = (body, status = 200) => route.fulfill({ status, contentType: 'application/json',
      headers: { 'x-config-generation': String(generation), 'x-config-epoch': 'fixture-epoch', etag: `"${current.revision}"` }, body: JSON.stringify(body) });
    if (url.pathname === '/') return route.fulfill({ contentType: 'text/html', body: html });
    if (path === '/config' && request.method() === 'GET') return reply(current);
    if (path.startsWith('/config') && request.method() !== 'GET') {
      const data = request.postDataJSON();
      if (path === '/config/revert') current = structuredClone(committed);
      else { current = structuredClone(data); if (data.committed) { current.revision++; committed = structuredClone(current); } }
      generation++; return reply(current);
    }
    if (path === '/projection/stats') return reply({ running: true, geometry: { warpAvailable: true, effectiveMode: current.projection.mode, retainedWarp: true } });
    if (path === '/projection/recommendation') {
      recommendationCalls++; inFlight++; maxFlight = Math.max(maxFlight, inFlight); await sleep(recommendationDelay); inFlight--;
      return reply({ revision: current.revision, generation, requestedAspect: current.projection.canvas.aspect,
        idealWidth: 8000, idealHeight: 4500, admissibleWidth: 8000, admissibleHeight: 4500,
        presets: [2000, 4000, 6000, 8000].map(width => ({ width, height: Math.round(width / current.projection.canvas.aspect) })),
        knownLimits: { maxDimension: 32768, maxCanvasPixels: 64000000 }, unknownLimits: [], warnings: [] });
    }
    if (path === '/outputs') return reply([]);
    if (path === '/av') return reply({ audioOutputs: [], audioInputs: [], videoInputs: [] });
    if (path === '/wallpapers') return reply([]);
    if (path === '/system') return reply({ suedeVersion: 'test', uptimeSeconds: 1, packages: [] });
    if (path === '/status') return reply({ divergences: [] });
    if (path === '/system/checks') return reply([]);
    return reply([]);
  });
  const page = await context.newPage(), failures = [];
  page.on('pageerror', error => failures.push(error.message));
  await page.goto('http://suede.test/?nosse');
  await page.waitForFunction(() => configGeneration === '0' || configGeneration === 0);
  await page.waitForSelector('#output-table tbody tr');
  const settle = () => page.evaluate(() => flushPreviews());
  const read = () => page.evaluate(() => structuredClone(config));

  assert.equal(await page.locator('#output-table tbody tr').count(), 4);
  await page.locator('#output-table tbody tr').nth(1).click();
  assert.equal(await page.locator('#output-name').textContent(), 'HDMI-A-2');
  const before = await read();
  await page.locator('#source-x').fill('2100.25'); await page.locator('#source-x').press('Tab'); await settle();
  let edited = await read();
  assert.equal(edited.outputs[1].geometry.source.x, 2100.25 / before.projection.canvas.renderWidth);
  assert.deepEqual(edited.outputs[1].geometry.corners, before.outputs[1].geometry.corners);
  assert.deepEqual(edited.outputs[1].geometry.rasterFootprint, before.outputs[1].geometry.rasterFootprint);

  const normalized = structuredClone(edited.outputs[1].geometry.source);
  await page.locator('#canvas-width').fill('4000'); await page.locator('#canvas-width').press('Tab'); await settle();
  edited = await read(); assert.deepEqual(edited.outputs[1].geometry.source, normalized);
  const oldCanvas = structuredClone(edited.projection.canvas), oldSource = structuredClone(edited.outputs[1].geometry.source);
  await page.locator('#canvas-aspect').fill('1.6'); await page.locator('#canvas-aspect').press('Tab'); await settle();
  edited = await read();
  const oldY = oldCanvas.aspect * Math.round(oldCanvas.renderWidth / oldCanvas.aspect);
  const newY = edited.projection.canvas.aspect * Math.round(edited.projection.canvas.renderWidth / edited.projection.canvas.aspect);
  assert(Math.abs(edited.outputs[1].geometry.source.y * newY - oldSource.y * oldY) < 1e-9);
  assert(Math.abs(edited.outputs[1].geometry.source.height * newY - oldSource.height * oldY) < 1e-9);

  await page.locator('#canvas-width').focus(); await page.waitForSelector('#canvas-presets:not([hidden]) button');
  assert.equal(await page.locator('#canvas-presets button').count(), 4);
  await page.locator('#canvas-presets button').filter({ hasText: '50%' }).click(); await settle();
  assert.equal((await read()).projection.canvas.renderWidth, 4000);
  recommendationDelay = 120;
  await page.locator('#content-scale').fill('175'); await page.locator('#content-scale').press('Tab');
  await page.locator('#content-scale').fill('180'); await page.locator('#content-scale').press('Tab');
  await sleep(500); await settle(); assert.equal(maxFlight, 1); assert(recommendationCalls >= 1);

  await page.locator('#arrange-outputs').click();
  await page.locator('#arrange-rows').fill('2'); await page.locator('#arrange-columns').fill('2');
  // High overlap warning when setting scale 50
  await page.locator('#arrange-scale').fill('50'); await page.locator('#arrange-scale').dispatchEvent('change');
  assert((await page.locator('#arrange-preview').textContent()).includes('Very high overlap') || (await page.locator('#arrange-preview').textContent()).includes('Overlap'));
  // Scale too small (< minScale for 100% overlap) disables Apply
  await page.locator('#arrange-scale').fill('20'); await page.locator('#arrange-scale').dispatchEvent('change');
  assert.equal(await page.locator('#arrange-apply').isDisabled(), true);
  assert((await page.locator('#arrange-preview').textContent()).includes('too small'));
  // Normal overlap 10% enables Apply and solves cleanly
  await page.locator('#arrange-overlap').fill('10'); await page.locator('#arrange-overlap').dispatchEvent('change');
  assert.equal(await page.locator('#arrange-apply').isDisabled(), false);
  await page.locator('#arrange-apply').click(); await settle();
  edited = await read(); assert(edited.outputs.every(output => output.geometry.source.width > 0 && output.geometry.source.height > 0));
  assert.deepEqual(edited.outputs[1].geometry.corners, before.outputs[1].geometry.corners);

  await page.evaluate(() => { projectionStats.geometry.warpAvailable = false; renderWarpEditor(); });
  assert.equal(await page.locator('#mode-simple').getAttribute('aria-pressed'), 'true');
  assert.equal(await page.locator('#mode-warp').isDisabled(), true);
  assert.equal((await read()).projection.mode, 'warp');
  await page.locator('#pj-save').click(); await page.waitForFunction(() => !configBusy && !previewActive);
  assert.equal(current.committed, true);
  await page.locator('#source-x').fill('5'); await page.locator('#source-x').press('Tab'); await settle();
  await page.locator('#pj-cancel').click(); await page.waitForFunction(() => !configBusy && !previewActive);
  assert.notEqual((await read()).outputs[1].geometry.source.x, 5 / (await read()).projection.canvas.renderWidth);
  assert.deepEqual(failures, []);
  if (process.env.UI_EVIDENCE) {
    fs.mkdirSync(process.env.UI_EVIDENCE, { recursive: true });
    await page.locator('#tab-displays').screenshot({ path: `${process.env.UI_EVIDENCE}/editor.png`, style: 'header, #toast { visibility: hidden !important; }' });
    fs.writeFileSync(`${process.env.UI_EVIDENCE}/browser.json`, JSON.stringify({ recommendationCalls, maxFlight, failures }, null, 2) + '\n');
  }
  await browser.close();
})().catch(error => { console.error(error); process.exit(1); });
