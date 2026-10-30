// Browser checks for Exact mode on the Displays tab: the mode toggle, the
// transition dry run and its confirm, the controls Exact hides or locks, and
// the px arrangement. Run from the repository root; see README.md in this
// directory. The fixture intercepts every request to suede.test and never
// connects to an appliance.
const assert = require('node:assert/strict');
const fs = require('node:fs');
const { chromium } = require('playwright');

const html = fs.readFileSync('src/api/ui/index.html', 'utf8');

// Four 1920x1080 outputs in a row on a fractional 8000 px Simple canvas.
function simpleDocument() {
  const fixture = JSON.parse(fs.readFileSync('docs/examples/four-output-warp.json', 'utf8'));
  for (const output of fixture.outputs) {
    if (output.geometry && Object.hasOwn(output.geometry, 'source')) {
      output.geometry.slice = output.geometry.source;
      delete output.geometry.source;
    }
  }
  fixture.projection.mode = 'simple';
  fixture.projection.canvas = { aspect: 8000 / 1080, renderWidth: 8000 };
  fixture.outputs.forEach((output, i) => {
    output.geometry.slice = { x: i * 1920 / 8000, y: 0, width: 1920 / 8000, height: 1080 / 8000 };
    output.geometry.rasterFootprint = structuredClone(output.geometry.slice);
  });
  return fixture;
}
// What the daemon's reset produces: the 7680x1080 bounding box, scale 1.
function resetDocument(from) {
  const next = structuredClone(from);
  next.projection.mode = 'exact';
  next.projection.canvas = { aspect: 7680 / 1080, renderWidth: 7680 };
  next.outputs.forEach((output, i) => {
    output.scale = 1;
    output.geometry.slice = { x: i * 1920 / 7680, y: 0, width: 1920 / 7680, height: 1080 / 7680 };
    output.geometry.rasterFootprint = structuredClone(output.geometry.slice);
  });
  return next;
}

(async () => {
  const browser = await chromium.launch({ headless: true });
  const failures = [];
  const puts = [], gets = [], arrangePuts = [];
  try {
    const context = await browser.newContext({ viewport: { width: 1280, height: 900 }, colorScheme: 'light' });
    const page = await context.newPage();
    page.on('pageerror', error => failures.push(error.message));
    await page.addInitScript(() => localStorage.setItem('suede.feeNoticeAcknowledged', '1'));
    const dialogs = [];
    let dialogAnswer = true;
    page.on('dialog', async dialog => { dialogs.push(dialog.message()); dialogAnswer ? await dialog.accept() : await dialog.dismiss(); });

    let current = simpleDocument();
    const names = current.outputs.map(o => o.match.name);
    let verdict = { action: 'reset', reasons: ['slices are not whole pixels'], revision: 0, generation: 0 };
    let verdictStatus = 200;
    let exactAvailable = true, exactReason = null;
    let rejectNextPut = null;
    const interval = {
      canvasFps: 60, presentedFps: 60, freeRun: false, renderer: 'gpu',
      captureIntervals: { one: 60, two: 0, three: 0, more: 0 }, offsetMs: null, straddles: 0,
      gateHolds: 0, framesSuperseded: 0, stalls: 0,
      perFrameMs: { waiting: 1, snapshot: 1, requesting: 1, blending: 1, gpu: 0 }, measuredAt: 1,
      outputs: names.map(name => ({ name, presented: 60, discarded: 0, zeroCopyPresented: 0, refreshHz: 60,
        phaseMs: null, phaseSpreadMs: null, lagFrames: { zero: 0, one: 0, two: 0, more: 0 }, commitToPresentMs: null })),
    };
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
      const headers = { 'x-config-generation': '0', 'x-config-epoch': 'exact-fixture', etag: '"0"' };
      if (path === '/config' && request.method() === 'PUT') {
        const body = request.postDataJSON();
        puts.push(body);
        if (rejectNextPut) {
          const detail = rejectNextPut; rejectNextPut = null;
          return respond({ detail }, headers, 422);
        }
        // The daemon's switching rules, as far as this page can see them.
        current = body.projection.mode === 'exact' && current.projection.mode !== 'exact' && verdict.action === 'reset'
          ? resetDocument(body) : structuredClone(body);
        return respond(current, headers);
      }
      if (path === '/config') return respond(current, headers);
      if (path === '/projection/exact-transition') {
        gets.push(path);
        return verdictStatus === 200 ? respond(verdict, headers) : respond({ detail: 'no output has a mode' }, headers, verdictStatus);
      }
      if (path === '/projection/arrangement' && request.method() === 'GET') {
        gets.push(`${path}${url.search}`);
        const overlapX = Number(url.searchParams.get('overlapX')), overlapY = Number(url.searchParams.get('overlapY'));
        const pxX = Math.round(overlapX * 1920), pxY = Math.round(overlapY * 1080);
        return respond({
          arrangement: { rows: 1, columns: 4, contentScale: 1, overlapX: pxX / 1920, overlapY: pxY / 1080 },
          unusedCanvas: { x: 0, y: 0 }, overhang: { x: 0, y: 0 }, impliedAspect: 1, outputs: [], warnings: [],
          canvas: { aspect: (4 * 1920 - 3 * pxX) / 1080, renderWidth: 4 * 1920 - 3 * pxX },
          overlapXPx: pxX, overlapYPx: pxY,
        }, headers);
      }
      if (path === '/config/projection/arrangement' && request.method() === 'PUT') {
        arrangePuts.push(request.postDataJSON());
        return respond(current, headers);
      }
      if (path === '/projection/stats') return respond({
        running: true, lastInterval: interval,
        geometry: { warpAvailable: true, effectiveMode: current.projection.mode, exactAvailable, exactReason },
        control: { session: 's', outputs: [] },
      });
      if (path === '/projection/recommendation') return respond({
        idealWidth: 7680, requestedAspect: 7680 / 1080, presets: [], warnings: [], approximate: false,
        generation: 0, revision: 0 }, headers);
      if (path === '/av') return respond({ audioOutputs: [], audioInputs: [], videoInputs: [] });
      if (path === '/system') return respond({ suedeVersion: 'test', uptimeSeconds: 1, packages: [] });
      if (path === '/status') return respond({ state: 'synced', divergences: [] });
      if (path === '/system/gpu') return respond({ available: true, gpus: [], processes: [] });
      if (path === '/system/cpu') return respond({});
      if (path === '/system/pipeline') return respond(null);
      return respond([]);
    });
    const refreshStats = () => page.evaluate(async () => { projectionStats = await api('/projection/stats'); renderWarpStatus(); });
    const settle = () => page.evaluate(() => flushPreviews());
    const visible = id => page.locator(`#${id}`).isVisible();

    await page.goto('http://suede.test/?nosse');
    await page.waitForFunction(() => config && configGeneration === '0');
    await refreshStats();
    await page.evaluate(() => selectOutput(config.outputs[0].match.name));

    // Toggle order, and the disabled state with the daemon's reason.
    assert.deepEqual(await page.locator('.mode-toggle[aria-label="Projection mode"] button').evaluateAll(b => b.map(x => x.id)),
      ['mode-exact', 'mode-simple', 'mode-warp']);
    exactAvailable = false; exactReason = 'exact mode needs the canvas slicer: this appliance tiles';
    await refreshStats();
    assert(await page.locator('#mode-exact').isDisabled());
    assert.equal(await page.locator('#mode-exact').getAttribute('title'), exactReason);
    exactAvailable = true; exactReason = null;
    await refreshStats();
    assert(await page.locator('#mode-exact').isEnabled());
    assert(await visible('pj-filter'), 'filter shown in Simple');
    assert(await visible('content-scale'));

    // An unavailable verdict and a 422 both block the switch and say why.
    verdict = { action: 'unavailable', reasons: ['output HDMI-A-9 has no mode'], revision: 0, generation: 0 };
    await page.locator('#mode-exact').click();
    await page.waitForFunction(() => document.getElementById('warp-error').textContent.includes('no mode'));
    assert.equal(puts.length, 0, 'nothing was written');
    verdictStatus = 422;
    await page.locator('#mode-exact').click();
    await page.waitForFunction(() => document.getElementById('warp-error').textContent === 'no output has a mode');
    assert.equal(puts.length, 0);
    verdictStatus = 200;

    // Reset: the confirm names the reasons; declining writes nothing.
    verdict = { action: 'reset', reasons: ['slices are not whole pixels'], revision: 0, generation: 0 };
    dialogAnswer = false;
    await page.locator('#mode-exact').click();
    await page.waitForFunction(() => true);
    await settle();
    assert.equal(dialogs.length, 1);
    assert.match(dialogs[0], /^Switching to Exact resets the layout: slices are not whole pixels\. Continue\?$/);
    assert.equal(puts.length, 0, 'declined: nothing written');
    assert.equal(await page.evaluate(() => config.projection.mode), 'simple');

    // Accepting previews the mode change only, then adopts the returned document.
    dialogAnswer = true;
    const before = await page.evaluate(() => structuredClone(config));
    await page.locator('#mode-exact').click();
    await page.waitForFunction(() => config.projection.mode === 'exact' && config.projection.canvas.renderWidth === 7680);
    assert.equal(dialogs.length, 2);
    assert.equal(puts.length, 1);
    assert.equal(puts[0].projection.mode, 'exact');
    assert.equal(puts[0].committed, false);
    assert.deepEqual({ ...puts[0].projection, mode: 'simple' }, before.projection, 'only the mode differs');
    assert.deepEqual(puts[0].outputs, before.outputs, 'no local geometry edit');
    assert.equal(await page.evaluate(() => config.outputs[1].scale), 1, 'the reset document was adopted');
    assert.equal(await page.locator('#mode-exact').getAttribute('aria-pressed'), 'true');
    assert.match(await page.locator('#warp-capability').textContent(),
      /^Exact: every output shows its canvas pixels one-to-one\. Scaling controls are unavailable\.$/);

    // Controls in Exact.
    assert(!(await visible('pj-filter')), 'filter hidden');
    assert(!(await visible('warp-controls')) && !(await visible('output-warp')), 'warp editor hidden');
    assert(!(await visible('canvas-presets')) && !(await visible('canvas-recommendation')) && !(await visible('canvas-rendering-scale')));
    assert(await page.locator('#canvas-width').evaluate(e => e.readOnly), 'canvas width read-only');
    assert.equal(await page.locator('#canvas-width').inputValue(), '7680');
    assert(await visible('canvas-fit'));
    assert(await page.locator('#o-scale').isDisabled());
    assert.match(await page.locator('#o-scale-hint').textContent(), /Fixed at 1\.0 in Exact mode/);
    assert.equal(await page.locator('#slice-x').getAttribute('step'), '1');
    assert(!(await visible('content-scale')), 'content scale hidden');
    assert.match(await page.locator('#source-size').textContent(), /Crop 1920 × 1080 px/);
    assert.match(await page.locator('#canvas-current').textContent(), /Exact mode: every output is one-to-one/);

    // A fractional crop is refused in the page; a daemon 422 rolls back verbatim.
    await page.evaluate(() => selectOutput(config.outputs[1].match.name));
    await page.locator('#slice-x').fill('1920.5');
    await page.locator('#slice-x').dispatchEvent('change');
    assert.match(await page.locator('#warp-error').textContent(), /Crop X must be a whole number of pixels in Exact mode/);
    assert.equal(puts.length, 1, 'nothing sent for a fractional crop');
    const reason = 'outputs[1].geometry.slice must be a whole-pixel rectangle of exactly 1920×1080 canvas pixels in exact mode';
    rejectNextPut = reason;
    await page.locator('#slice-x').fill('1930');
    await page.locator('#slice-x').dispatchEvent('change');
    await settle();
    assert.equal(await page.locator('#warp-error').textContent(), reason);
    assert.equal(await page.evaluate(() => Math.round(config.outputs[1].geometry.slice.x * 7680)), 1920, 'rolled back');
    await page.locator('#slice-x').fill('1920');
    await page.locator('#slice-x').blur();
    await settle();

    // Fit canvas to outputs: shifts the slices so the box starts at the origin.
    await page.evaluate(() => {
      config.outputs.forEach(o => { o.geometry.slice.x += 100 / 7680; });
      config.projection.canvas = { aspect: 8000 / 1080, renderWidth: 8000 };
      for (const o of config.outputs) { const s = o.geometry.slice; s.x = s.x * 7680 / 8000; s.width = 1920 / 8000; s.height = 1080 / 8000; }
    });
    const putsBeforeFit = puts.length;
    await page.locator('#canvas-fit').click();
    await settle();
    const fit = puts.at(putsBeforeFit);
    assert.equal(fit.projection.canvas.renderWidth, 7680);
    assert(Math.abs(fit.projection.canvas.aspect - 7680 / 1080) < 1e-12);
    assert.deepEqual(fit.outputs.map(o => Math.round(o.geometry.slice.x * 7680)), [0, 1920, 3840, 5760]);

    // Arrangement: px in, normalized per-axis fractions out, px back.
    await page.locator('#arrange-outputs').click();
    await page.waitForFunction(() => document.getElementById('arrange-preview').textContent.includes('Canvas derived'));
    assert(!(await visible('arrange-mode-row')), 'overlap/scale toggle hidden');
    assert(!(await visible('arrange-scale')));
    assert.equal((await page.locator('#arrange-scale-fixed output').textContent()).trim(), '100%');
    assert.equal(await page.locator('label[for="arrange-overlap-x"]').textContent(), 'Overlap X (px)');
    assert(await visible('arrange-exact-note'));
    await page.locator('#arrange-columns').fill('4');
    await page.locator('#arrange-overlap-x').fill('160');
    await page.locator('#arrange-overlap-x').dispatchEvent('change');
    await page.locator('#arrange-overlap-y').fill('0');
    await page.locator('#arrange-overlap-y').dispatchEvent('change');
    await page.waitForFunction(() => document.getElementById('arrange-preview').textContent.includes('Overlap: 160 px H'));
    const dry = new URLSearchParams(gets.filter(g => g.startsWith('/projection/arrangement?')).at(-1).split('?')[1]);
    assert.equal(Number(dry.get('overlapX')), 160 / 1920);
    assert.equal(Number(dry.get('overlapY')), 0);
    assert(!dry.has('contentScale'));
    assert.match(await page.locator('#arrange-preview').textContent(), /Canvas derived from the arrangement: 7200 × 1080 px/);
    await page.locator('#arrange-overlap-y').fill('160');
    await page.locator('#arrange-overlap-y').dispatchEvent('change');
    await page.waitForTimeout(500);
    const dry2 = new URLSearchParams(gets.filter(g => g.startsWith('/projection/arrangement?')).at(-1).split('?')[1]);
    assert.equal(Number(dry2.get('overlapY')), 0, 'a single row has no vertical seam');
    await page.locator('#arrange-apply').click();
    await page.waitForFunction(() => !document.getElementById('arrange-dialog').open);
    assert.equal(arrangePuts.at(-1).overlapX, 160 / 1920);
    assert(!Object.hasOwn(arrangePuts.at(-1), 'contentScale'));
    await page.locator('#arrange-outputs').click();
    await page.waitForFunction(() => document.getElementById('arrange-dialog').open);
    await page.locator('#arrange-cancel').click();

    // Back to Simple: the controls return and only the mode is pushed.
    const putsBeforeLeave = puts.length;
    await page.locator('#mode-simple').click();
    await page.waitForFunction(() => config.projection.mode === 'simple');
    await settle();
    assert.equal(puts.at(putsBeforeLeave).projection.mode, 'simple');
    assert(await visible('pj-filter'));
    assert(await visible('content-scale'));
    assert(await page.locator('#o-scale').isEnabled());
    assert(!(await page.locator('#canvas-width').evaluate(e => e.readOnly)));
    assert(!(await visible('canvas-fit')));
    assert.equal(await page.locator('#slice-x').getAttribute('step'), 'any');
    assert(await visible('arrange-outputs'));
    await page.locator('#arrange-outputs').click();
    assert(await visible('arrange-mode-row'));
    assert(await visible('arrange-scale'));
    assert(!(await visible('arrange-scale-fixed')));
    assert.equal(await page.locator('label[for="arrange-overlap-x"]').textContent(), 'Overlap X (%)');
    await page.locator('#arrange-cancel').click();

    // A conforming document needs no confirm.
    verdict = { action: 'keep', reasons: [], revision: 0, generation: 0 };
    const dialogsBefore = dialogs.length;
    await page.locator('#mode-exact').click();
    await page.waitForFunction(() => config.projection.mode === 'exact');
    await settle();
    assert.equal(dialogs.length, dialogsBefore, 'keep never asks');
  } catch (error) {
    failures.push(error.stack || String(error));
  } finally {
    await browser.close();
  }
  if (failures.length) { console.error(failures.join('\n')); process.exit(1); }
  console.log('exact mode UI checks passed');
})();
