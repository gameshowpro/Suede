// Browser checks for the whole-number chip: when the Displays tab shows it,
// what its label and tooltip say, the muted hint in the output editor, and
// the cases where it must stay silent. Run from the repository root; see
// README.md in this directory. The fixture intercepts every request to
// suede.test and never connects to an appliance.
const assert = require('node:assert/strict');
const fs = require('node:fs');
const { chromium } = require('playwright');

const html = fs.readFileSync('src/api/ui/index.html', 'utf8');
const HELD = 3;

function document() {
  const fixture = JSON.parse(fs.readFileSync('docs/examples/four-output-warp.json', 'utf8'));
  for (const output of fixture.outputs) {
    if (output.geometry && Object.hasOwn(output.geometry, 'source')) {
      output.geometry.slice = output.geometry.source;
      delete output.geometry.source;
    }
  }
  fixture.projection.mode = 'simple';
  fixture.projection.canvas = { aspect: 8000 / 1080, renderWidth: 8000 };
  fixture.projection.sliceUnits = 'pixels';
  fixture.outputs.forEach((output, i) => {
    output.geometry.slice = { x: i * 1920, y: 0, width: 1920, height: 1080 };
    output.geometry.rasterFootprint = structuredClone(output.geometry.slice);
  });
  return fixture;
}

const mapping = (over = {}) => ({
  ratioX: 1, ratioY: 1, wholeNumber: true, samplingPath: 'exact', transformOwner: 'slicer',
  transform: 'normal', outputScale: 1, browserScaleFactor: 1, verified: true, unverified: [],
  generation: HELD, ...over,
});

(async () => {
  const browser = await chromium.launch({ headless: true });
  const failures = [];
  try {
    const context = await browser.newContext({ viewport: { width: 1280, height: 900 }, colorScheme: 'light' });
    const page = await context.newPage();
    page.on('pageerror', error => failures.push(error.message));
    await page.addInitScript(() => localStorage.setItem('suede.feeNoticeAcknowledged', '1'));

    const current = document();
    const names = current.outputs.map(o => o.match.name);
    let control = { session: 's', outputs: [] };
    await page.route('**/*', async route => {
      const request = route.request();
      const url = new URL(request.url());
      const respond = (body, headers = {}, status = 200) => route.fulfill({ status, contentType: 'application/json',
        headers, body: JSON.stringify(body) });
      if (url.pathname === '/') return route.fulfill({ contentType: 'text/html', body: html });
      if (url.pathname === '/assets/fonts/fonts.css') return route.fulfill({
        contentType: 'text/css', body: fs.readFileSync('fonts/fonts.css') });
      if (url.pathname.startsWith('/assets/fonts/') && url.pathname.endsWith('.woff2')) return route.fulfill({
        contentType: 'font/woff2', body: fs.readFileSync(`fonts/${url.pathname.slice('/assets/fonts/'.length)}`) });
      const path = url.pathname.replace('/api/v1', '');
      const headers = { 'x-config-generation': String(HELD), 'x-config-epoch': 'chip-fixture', etag: `"${HELD}"` };
      if (path === '/config') return respond(current, headers);
      if (path === '/projection/stats') return respond({
        running: true,
        geometry: { warpAvailable: true, effectiveMode: 'simple', exactAvailable: true, exactReason: null },
        control,
      });
      if (path === '/projection/recommendation') return respond({
        idealWidth: 8000, requestedAspect: 8000 / 1080, presets: [], warnings: [], approximate: false,
        generation: HELD, revision: 0 }, headers);
      if (path === '/av') return respond({ audioOutputs: [], audioInputs: [], videoInputs: [] });
      if (path === '/system') return respond({ suedeVersion: 'test', uptimeSeconds: 1, packages: [] });
      if (path === '/status') return respond({ state: 'synced', divergences: [] });
      if (path === '/system/gpu') return respond({ available: true, gpus: [], processes: [] });
      if (path === '/system/cpu') return respond({});
      if (path === '/system/pipeline') return respond(null);
      return respond([]);
    });
    const refresh = () => page.evaluate(async () => {
      projectionStats = await api('/projection/stats'); renderLayout(); renderWarpStatus(); renderCanvas();
    });
    const chips = () => page.locator('#layout .mapping-chip');
    const chipTexts = () => chips().allTextContents();
    const canvasLine = () => page.locator('#canvas-current').textContent();

    await page.goto('http://suede.test/?nosse');
    await page.waitForFunction(g => config && String(configGeneration) === g, String(HELD));

    // No control block entries: no chip, and no one-to-one claim anywhere.
    await refresh();
    assert.equal(await chips().count(), 0);
    assert.doesNotMatch(await canvasLine(), /1:1|one-to-one|whole-number/);

    // A stopped slicer: the mapping is gone, so nothing is shown, even though
    // other fields of the last session may linger.
    control = { session: 's', outputs: names.map(name => ({ name, samplingMode: 'exact', presentedGeneration: HELD })) };
    await refresh();
    assert.equal(await chips().count(), 0, 'no mapping, no chip');
    await page.evaluate(n => selectOutput(n), names[0]);
    assert.equal(await page.locator('#output-mapping-chip .mapping-chip').count(), 0);
    assert(await page.locator('#output-mapping-hint').isHidden());
    assert.doesNotMatch(await canvasLine(), /1:1|one-to-one|whole-number|not verified/);

    // One verified, one composited-rotated, one fractional, one lagging.
    control = {
      session: 's',
      capture: { kind: 'dmabuf', format: 'XR24', modifier: '0x0', converted: false },
      outputs: [
        { name: names[0], mapping: mapping() },
        { name: names[1], mapping: mapping({ transformOwner: 'compositor', transform: '180', verified: false,
          unverified: ['rotated by the compositor'] }) },
        { name: names[2], mapping: mapping({ wholeNumber: false, ratioX: 1.5, ratioY: 1.5, verified: false,
          unverified: ['scaling 1.5× is not a whole number', 'fractional crop origin'] }) },
        { name: names[3], mapping: mapping({ generation: HELD - 1 }) },
      ],
    };
    await refresh();
    assert.deepEqual(await chipTexts(), ['1:1 verified'], 'exactly one chip: unverified and lagging outputs show none');
    assert.equal(await page.locator(`#layout .display-box[data-name="${names[0]}"] .mapping-chip`).count(), 1);
    const tip = await chips().first().getAttribute('title');
    for (const line of ['Ratio: 1×', 'Sampling path: exact', 'Transform owner: slicer (normal)',
      'Output scale: 1', 'Browser scale factor: 1', 'Capture: dmabuf XR24 0x0 not converted']) {
      assert(tip.includes(line), `tooltip has "${line}": ${tip}`);
    }
    // The canvas line follows the selected output.
    assert.match(await canvasLine(), /Selected output: 1:1 verified\./);
    assert.equal(await page.locator('#output-mapping-chip .mapping-chip').count(), 1, 'editor chip for the selected output');
    assert(await page.locator('#output-mapping-hint').isHidden(), 'no hint on a verified output');

    // The hint lists unverified[] verbatim in the editor only.
    await page.evaluate(n => selectOutput(n), names[1]);
    assert.equal(await page.locator('#output-mapping-chip .mapping-chip').count(), 0);
    assert.equal(await page.locator('#output-mapping-hint').textContent(),
      'Not verified as whole-number scaling: rotated by the compositor.');
    assert.match(await canvasLine(), /Selected output: not verified as whole-number scaling\./);
    await page.evaluate(n => selectOutput(n), names[2]);
    assert.equal(await page.locator('#output-mapping-hint').textContent(),
      'Not verified as whole-number scaling: scaling 1.5× is not a whole number; fractional crop origin.');
    assert.equal(await page.locator('#layout .display-box .hint, #layout .display-box [class*="unverified"]').count(), 0,
      'the layout never shows the reasons');
    // A lagging verdict: neither chip nor claim for that output.
    await page.evaluate(n => selectOutput(n), names[3]);
    assert.equal(await page.locator('#output-mapping-chip .mapping-chip').count(), 0);
    assert.doesNotMatch(await canvasLine(), /1:1|not verified/);

    // The requested generation moves ahead: every chip and hint disappears.
    await page.evaluate(n => selectOutput(n), names[0]);
    assert.equal(await chips().count(), 1);
    await page.evaluate(() => { configGeneration = String(Number(configGeneration) + 1); renderLayout(); renderWarpStatus(); });
    assert.equal(await chips().count(), 0, 'hidden while the requested generation is newer');
    assert.equal(await page.locator('#output-mapping-chip .mapping-chip').count(), 0);
    await page.evaluate(n => selectOutput(n), names[1]);
    assert(await page.locator('#output-mapping-hint').isHidden(), 'hint hidden while pending');
    assert.doesNotMatch(await canvasLine(), /1:1|not verified/);
    await page.evaluate(g => { configGeneration = g; renderLayout(); renderWarpStatus(); }, String(HELD));
    assert.equal(await chips().count(), 1, 'back in step: chip returns');

    // Whole-number labels.
    control = {
      session: 's', capture: { kind: 'shm', converted: false },
      outputs: [
        { name: names[0], mapping: mapping({ ratioX: 2, ratioY: 2, samplingPath: 'nearest' }) },
        { name: names[1], mapping: mapping({ ratioX: 2, ratioY: 3, samplingPath: 'nearest' }) },
        { name: names[2], mapping: mapping({ ratioX: 1, ratioY: 1, samplingPath: 'bilinear', verified: false,
          unverified: ['bilinear sampling at 1× instead of the exact copy'] }) },
      ],
    };
    await refresh();
    assert.deepEqual(await chipTexts(), ['2× whole-number verified', '2×3× whole-number verified']);
    assert((await chips().first().getAttribute('title')).includes('Sampling path: nearest'));
    assert((await chips().first().getAttribute('title')).includes('Capture: shm not converted'));

    // The tiled path: only name and mapping, no capture.
    control = {
      session: null,
      outputs: names.map(name => ({ name, mapping: mapping({ samplingPath: 'tiled', transformOwner: 'compositor' }) })),
    };
    await refresh();
    assert.deepEqual(await chipTexts(), Array(names.length).fill('1:1 (tiled)'));
    assert((await chips().first().getAttribute('title')).includes('Capture: not applicable'));

    // The slicer stops: the mapping is removed and so is the chip.
    control = { session: null, outputs: [] };
    await refresh();
    assert.equal(await chips().count(), 0);
    assert.doesNotMatch(await canvasLine(), /1:1|one-to-one|whole-number/);

    assert.deepEqual(failures, [], 'no page errors');
    console.log('chip: ok');
  } finally {
    await browser.close();
  }
})().catch(error => { console.error(error); process.exit(1); });
