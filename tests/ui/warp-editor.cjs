// Real Chromium checks for the consolidated Displays editor.
const { chromium } = require('playwright');
const fs = require('node:fs');
const assert = require('node:assert/strict');
const fixture = JSON.parse(fs.readFileSync('docs/examples/four-output-warp.json'));
// The example document on disk still uses the pre-rename field name
// (`geometry.source`); the page's rename to `geometry.slice` is proven here
// independently of the parallel Rust/doc renames, so translate the fixture's
// own in-memory copy rather than touching the shared example file.
for (const output of fixture.outputs ?? []) {
  if (output.geometry && Object.prototype.hasOwnProperty.call(output.geometry, 'source')) {
    output.geometry.slice = output.geometry.source;
    delete output.geometry.source;
  }
}
const html = fs.readFileSync('src/api/ui/index.html', 'utf8');
const sleep = ms => new Promise(resolve => setTimeout(resolve, ms));
// For state that lives in this file's own route handler (the fixture), not
// in the page: polling here, rather than page.waitForFunction, since that
// state is never exposed to the page.
const waitFor = async (predicate, timeoutMs = 3000) => {
  const start = Date.now();
  while (!predicate()) {
    if (Date.now() - start > timeoutMs) throw new Error('timed out waiting for condition');
    await sleep(20);
  }
};

// The grid solver lives in the daemon and is proven by Rust tests, so this
// fixture only answers the two arrangement endpoints the way the daemon
// would; the browser check proves the wiring, not the arithmetic. The canned
// numbers are a real solve of the fixture's four 1920x1080 outputs as a 2x2
// at 10%/10% overlap against the canvas the scenario has by then (4000 wide,
// aspect 1.6, so 4000x2500 with a y density of 4000). This first set is
// specifically the allowUnusedCanvas:true request (fit the grid inside the
// canvas, scale = max(fitX, fitY)): X binds at content scale 0.912, and the
// vertically centered grid leaves a tenth of the canvas unused, which a
// 16:9 canvas would fill.
const ARRANGED_SCALE = 0.912;
const arrangedOrigins = { x: [0, (1920 - 192) / ARRANGED_SCALE], y: [125, 125 + (1080 - 108) / ARRANGED_SCALE] };
const arrangedSources = [0, 1, 2, 3].map(index => ({
  x: arrangedOrigins.x[index % 2] / 4000,
  y: arrangedOrigins.y[Math.floor(index / 2)] / 4000,
  width: 1920 / (ARRANGED_SCALE * 4000),
  height: 1080 / (ARRANGED_SCALE * 4000),
}));
// Same four outputs, same 10%/10% overlap, but with allowUnusedCanvas left
// false (the exact+overfill solver's default, and the only kind of answer an
// overlap request without the flag ever gets now): the grid instead covers
// its tighter axis exactly and overhangs the other, centered
// (scale = min(fitX, fitY)). fitX = (1920*2 - 192)/4000 = 0.912 (same fit as
// above); fitY = (1080*2 - 108)/2500 = 0.8208. Y is tighter, so it binds
// (no unused/overhang on Y) and X overhangs by fitX/scale - 1 =
// 0.912/0.8208 - 1 = 1/9. The overhang is centered, so each column's
// canvas-pixel origin is shifted left by half of it, in the same
// 4000-wide-canvas pixel density arrangedOrigins uses:
// margin = -(4000 * 1/9) / 2 = -2000/9.
const OVERHANG_SCALE = (1080 * 2 - 108) / 2500;
const OVERHANG_X = (1920 * 2 - 192) / 4000 / OVERHANG_SCALE - 1;
const overhangMarginX = -(4000 * OVERHANG_X) / 2;
const overhangOrigins = {
  x: [0 + overhangMarginX, (1920 - 192) / OVERHANG_SCALE + overhangMarginX],
  y: [0, (1080 - 108) / OVERHANG_SCALE],
};
const overhangSources = [0, 1, 2, 3].map(index => ({
  x: overhangOrigins.x[index % 2] / 4000,
  y: overhangOrigins.y[Math.floor(index / 2)] / 4000,
  width: 1920 / (OVERHANG_SCALE * 4000),
  height: 1080 / (OVERHANG_SCALE * 4000),
}));
const veryHighOverlap = 'Overlap is very high (>80%); outputs almost entirely cover each other.';

(async () => {
  const browser = await chromium.launch({ headless: true });
  let current = structuredClone(fixture), committed = structuredClone(fixture), generation = 0;
  let recommendationCalls = 0, recommendationDelay = 0, inFlight = 0, maxFlight = 0;
  const arrangeQueries = [], arrangePuts = [], arrangeQueryOutputOrders = [];
  const context = await browser.newContext({ viewport: { width: 1280, height: 1000 } });
  await context.route('**/*', async route => {
    const request = route.request(), url = new URL(request.url()), path = url.pathname.replace('/api/v1', '');
    const reply = (body, status = 200) => route.fulfill({ status, contentType: 'application/json',
      headers: { 'x-config-generation': String(generation), 'x-config-epoch': 'fixture-epoch', etag: `"${current.revision}"` }, body: JSON.stringify(body) });
    if (url.pathname === '/') return route.fulfill({ contentType: 'text/html', body: html });
    if (path === '/config' && request.method() === 'GET') return reply(current);
    // The record always holds all five resolved values, whichever family the
    // request used, so an absent field falls back rather than erasing one.
    const resolved = (rows, columns, values) => ({
      rows, columns, overlapX: 0, overlapY: 0, contentScale: ARRANGED_SCALE,
      ...Object.fromEntries(Object.entries(values).filter(([, value]) => value !== undefined)),
    });
    // `geometry` defaults to the allowUnusedCanvas:true/band case above
    // (ARRANGED_SCALE, unusedCanvas.y = 0.1); the overhang case below passes
    // its own unusedCanvas/overhang/slices instead. `limits` is omitted
    // here as it is against an older daemon: the reference UI doesn't
    // consume it yet either (a deliberately deferred follow-up, per the
    // plan), so nothing exercises it and a hand-derived value would just be
    // unverified noise.
    const solution = (rows, columns, values, warnings,
      geometry = { unusedCanvas: { x: 0, y: 0.1 }, overhang: { x: 0, y: 0 }, slices: arrangedSources }) => ({
      arrangement: resolved(rows, columns, values),
      unusedCanvas: geometry.unusedCanvas, overhang: geometry.overhang, impliedAspect: 16 / 9,
      outputs: geometry.slices.map((slice, index) => ({ key: current.outputs[index].match.name, slice })),
      warnings, revision: current.revision, generation,
    });
    if (path === '/projection/arrangement' && request.method() === 'GET') {
      const query = Object.fromEntries(url.searchParams);
      arrangeQueries.push(query);
      // Proves `requestArrangement`'s own contract (it flushes previews
      // before asking the daemon): the order the solver sees at request time
      // must already be whatever config.outputs held when this fired.
      arrangeQueryOutputOrders.push(current.outputs.map(output => output.match?.name));
      const rows = Number(query.rows), columns = Number(query.columns);
      if (query.contentScale !== undefined) {
        // Too coarse for the grid to reach the canvas edges at all.
        if (Number(query.contentScale) <= 0.2)
          return reply({ title: 'Unprocessable Entity', status: 422, detail: 'content scale is too small for this grid' }, 422);
        // Scale mode derives both overlaps; this one needs most of each
        // output sitting on top of its neighbor.
        return reply(solution(rows, columns,
          { contentScale: Number(query.contentScale), overlapX: 0.9, overlapY: 0.86 }, [veryHighOverlap]));
      }
      const overlapX = Number(query.overlapX ?? 0), overlapY = Number(query.overlapY ?? 0);
      // The 10%/10% case is this fixture's one canned pair of overlap
      // answers (see OVERHANG_SCALE/ARRANGED_SCALE above): with
      // allowUnusedCanvas left false (the daemon's own default and the
      // exact+overfill solver's only overlap-mode answer now) the grid
      // overhangs X instead of refusing — the old floors-era 422 for this
      // combination is gone, since a plain band is no longer a refusal
      // reason. Ticking the box resends the same numbers with the flag true
      // and fits the grid inside the canvas instead, trading the overhang
      // for the band.
      if (overlapX === 0.1 && overlapY === 0.1 && query.allowUnusedCanvas !== 'true') {
        return reply(solution(rows, columns, { overlapX, overlapY, contentScale: OVERHANG_SCALE }, [],
          { unusedCanvas: { x: 0, y: 0 }, overhang: { x: OVERHANG_X, y: 0 }, slices: overhangSources }));
      }
      return reply(solution(rows, columns, { overlapX, overlapY },
        overlapX > 0.8 || overlapY > 0.8 ? [veryHighOverlap] : []));
    }
    if (path === '/config/projection/arrangement' && request.method() === 'PUT') {
      // Applying replaces every enabled output's slice and records the
      // resolved arrangement, exactly like the daemon; the page is loaded
      // with ?nosse, so there is no event stream to publish config_changed
      // on and the response is the whole story.
      const data = request.postDataJSON();
      arrangePuts.push(data);
      current = structuredClone(current);
      current.outputs.forEach((output, index) => { output.geometry.slice = structuredClone(arrangedSources[index]); });
      current.projection.arrangement = resolved(data.rows, data.columns,
        { overlapX: data.overlapX, overlapY: data.overlapY, contentScale: data.contentScale ?? ARRANGED_SCALE });
      current.committed = Boolean(data.committed);
      generation++; return reply(current);
    }
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
  await page.locator('#slice-x').fill('2100.25'); await page.locator('#slice-x').press('Tab'); await settle();
  let edited = await read();
  assert.equal(edited.outputs[1].geometry.slice.x, 2100.25 / before.projection.canvas.renderWidth);
  assert.deepEqual(edited.outputs[1].geometry.corners, before.outputs[1].geometry.corners);
  assert.deepEqual(edited.outputs[1].geometry.rasterFootprint, before.outputs[1].geometry.rasterFootprint);

  const normalized = structuredClone(edited.outputs[1].geometry.slice);
  await page.locator('#canvas-width').fill('4000'); await page.locator('#canvas-width').press('Tab'); await settle();
  edited = await read(); assert.deepEqual(edited.outputs[1].geometry.slice, normalized);
  const oldCanvas = structuredClone(edited.projection.canvas), oldSlice = structuredClone(edited.outputs[1].geometry.slice);
  await page.locator('#canvas-aspect').fill('1.6'); await page.locator('#canvas-aspect').press('Tab'); await settle();
  edited = await read();
  const oldY = oldCanvas.aspect * Math.round(oldCanvas.renderWidth / oldCanvas.aspect);
  const newY = edited.projection.canvas.aspect * Math.round(edited.projection.canvas.renderWidth / edited.projection.canvas.aspect);
  assert(Math.abs(edited.outputs[1].geometry.slice.y * newY - oldSlice.y * oldY) < 1e-9);
  assert(Math.abs(edited.outputs[1].geometry.slice.height * newY - oldSlice.height * oldY) < 1e-9);

  await page.locator('#canvas-width').focus(); await page.waitForSelector('#canvas-presets:not([hidden]) button');
  assert.equal(await page.locator('#canvas-presets button').count(), 4);
  await page.locator('#canvas-presets button').filter({ hasText: '50%' }).click(); await settle();
  assert.equal((await read()).projection.canvas.renderWidth, 4000);
  recommendationDelay = 120;
  await page.locator('#content-scale').fill('175'); await page.locator('#content-scale').press('Tab');
  await page.locator('#content-scale').fill('180'); await page.locator('#content-scale').press('Tab');
  await sleep(500); await settle(); assert.equal(maxFlight, 1); assert(recommendationCalls >= 1);

  // The arrangement dialog holds no solver: every line it shows is the
  // daemon's answer to a dry run, debounced and latest-wins.
  const arrangeSays = text => page.waitForFunction(
    expected => document.getElementById('arrange-preview').textContent.includes(expected), text);
  await page.locator('#arrange-outputs').click();
  await page.locator('#arrange-rows').fill('2'); await page.locator('#arrange-columns').fill('2');

  // "Use all canvas pixels" is the default side, and the flag is sent on
  // every dry run either way (a no-op in scale mode, but the daemon's own
  // default is also false).
  assert.equal(await page.locator('#arrange-coverage-all').getAttribute('aria-pressed'), 'true');
  assert.equal(await page.locator('#arrange-coverage-inside').getAttribute('aria-pressed'), 'false');

  // The order list is config.outputs order itself, one row per configured
  // output, position first. Bumping the second row above the first is an
  // ordinary preview edit to config.outputs — not something the dialog
  // computes — so it shows up in the fixture's own document once flushed.
  assert.deepEqual(await page.locator('#arrange-order .arrange-order-name').allTextContents(),
    ['HDMI-A-1', 'HDMI-A-2', 'HDMI-A-3', 'HDMI-A-4']);
  const reordered = ['HDMI-A-2', 'HDMI-A-1', 'HDMI-A-3', 'HDMI-A-4'];
  await page.locator('#arrange-order li').nth(1).getByRole('button', { name: 'Move HDMI-A-2 up' }).click();
  await settle();
  assert.deepEqual(await page.locator('#arrange-order .arrange-order-name').allTextContents(), reordered);
  // flushPreviews() inside settle() only resolves once the fixture's own
  // document has the swap, so this is not a race.
  assert.deepEqual(current.outputs.map(output => output.match.name), reordered);
  // The next dry run — fired off the debounce the bump (re)armed — solves
  // against the reordered document, because requestArrangement() flushes
  // previews before asking the daemon.
  await waitFor(() => arrangeQueryOutputOrders.at(-1)?.join() === reordered.join());
  assert.equal(arrangeQueries.at(-1).allowUnusedCanvas, 'false');

  // Overlap is the default mode: overlap inputs live, scale is the daemon's
  // to fill in. Only the selected family is ever sent (see the query/PUT
  // assertions below).
  assert.equal(await page.locator('#arrange-mode-overlap').getAttribute('aria-pressed'), 'true');
  assert.equal(await page.locator('#arrange-mode-scale').getAttribute('aria-pressed'), 'false');
  assert.equal(await page.locator('#arrange-overlap-x').isDisabled(), false);
  assert.equal(await page.locator('#arrange-overlap-y').isDisabled(), false);
  assert.equal(await page.locator('#arrange-scale').isDisabled(), true);

  // Switching to Scale flips which family is live; a scale edit is what
  // fills both overlap fields, since they are the daemon's to write now.
  await page.locator('#arrange-mode-scale').click();
  assert.equal(await page.locator('#arrange-mode-overlap').getAttribute('aria-pressed'), 'false');
  assert.equal(await page.locator('#arrange-mode-scale').getAttribute('aria-pressed'), 'true');
  assert.equal(await page.locator('#arrange-scale').isDisabled(), false);
  assert.equal(await page.locator('#arrange-overlap-x').isDisabled(), true);
  assert.equal(await page.locator('#arrange-overlap-y').isDisabled(), true);
  // Scale 50 is answered with derived overlaps and a warning.
  await page.locator('#arrange-scale').fill('50'); await page.locator('#arrange-scale').dispatchEvent('change');
  await arrangeSays('Overlap is very high');
  assert.equal(await page.locator('#arrange-overlap-x').inputValue(), '90');
  assert.equal(await page.locator('#arrange-overlap-y').inputValue(), '86');
  assert.deepEqual(arrangeQueries.at(-1), { rows: '2', columns: '2', contentScale: '0.5', allowUnusedCanvas: 'false' });
  // A refused solve shows the daemon's detail and disables Apply
  await page.locator('#arrange-scale').fill('20'); await page.locator('#arrange-scale').dispatchEvent('change');
  await arrangeSays('too small');
  assert.equal(await page.locator('#arrange-apply').isDisabled(), true);

  // Back to Overlap: 10% on each axis, both are sent, Content scale comes
  // back from the answer, and — with allowUnusedCanvas left unchecked, the
  // default — the exact+overfill solver's overhang on the tighter axis is
  // reported instead of the retired floors-era band refusal.
  await page.locator('#arrange-mode-overlap').click();
  assert.equal(await page.locator('#arrange-overlap-x').isDisabled(), false);
  assert.equal(await page.locator('#arrange-scale').isDisabled(), true);
  await page.locator('#arrange-overlap-x').fill('10'); await page.locator('#arrange-overlap-x').dispatchEvent('change');
  await page.locator('#arrange-overlap-y').fill('10'); await page.locator('#arrange-overlap-y').dispatchEvent('change');
  // Unchecked (the default): the daemon no longer refuses this combination —
  // it covers Y exactly and overhangs X by 1/9 (11.1%, see OVERHANG_X above)
  // instead, so Apply stays enabled.
  await arrangeSays('overhangs the canvas horizontally');
  assert.equal(await page.locator('#arrange-scale').inputValue(), '82.08');
  assert.equal(await page.locator('#arrange-apply').isDisabled(), false);
  assert.deepEqual(arrangeQueries.at(-1),
    { rows: '2', columns: '2', overlapX: '0.1', overlapY: '0.1', allowUnusedCanvas: 'false' });

  // Clicking "Keep slices inside the canvas" resends the same numbers with
  // the flag true; the daemon now fits the grid inside the canvas instead
  // (the band case), trading the horizontal overhang for a vertical band.
  // "Overlap: 10% H, 10% V" is already in the preview text from the
  // overhang answer above, so "unused vertically" (only ever true for the
  // band answer) is the wait that actually proves the new response landed.
  await page.locator('#arrange-coverage-inside').click();
  assert.equal(await page.locator('#arrange-coverage-all').getAttribute('aria-pressed'), 'false');
  assert.equal(await page.locator('#arrange-coverage-inside').getAttribute('aria-pressed'), 'true');
  await arrangeSays('unused vertically');
  assert.equal(await page.locator('#arrange-scale').inputValue(), '91.2');
  assert.equal(await page.locator('#arrange-apply').isDisabled(), false);
  assert.deepEqual(arrangeQueries.at(-1),
    { rows: '2', columns: '2', overlapX: '0.1', overlapY: '0.1', allowUnusedCanvas: 'true' });
  await page.locator('#arrange-apply').click();
  await page.waitForFunction(() => !document.getElementById('arrange-dialog').open);
  await settle();
  edited = await read();
  assert.deepEqual(arrangePuts.at(-1),
    { rows: 2, columns: 2, overlapX: 0.1, overlapY: 0.1, allowUnusedCanvas: true, committed: false });
  // The page adopts the daemon's document rather than arranging anything
  // itself, so the slices are exactly the ones it was sent, in the bumped
  // order the dialog now shows.
  assert.deepEqual(edited.outputs.map(output => output.geometry.slice), arrangedSources);
  assert.deepEqual(edited.outputs.map(output => output.match.name),
    ['HDMI-A-2', 'HDMI-A-1', 'HDMI-A-3', 'HDMI-A-4']);
  assert.deepEqual(edited.projection.arrangement,
    { rows: 2, columns: 2, overlapX: 0.1, overlapY: 0.1, contentScale: ARRANGED_SCALE });
  assert.deepEqual(edited.outputs.find(output => output.match.name === 'HDMI-A-2').geometry.corners,
    before.outputs[1].geometry.corners);

  // Reopening the dialog remembers the toggle choice, same as the mode.
  await page.locator('#arrange-outputs').click();
  assert.equal(await page.locator('#arrange-coverage-all').getAttribute('aria-pressed'), 'false');
  assert.equal(await page.locator('#arrange-coverage-inside').getAttribute('aria-pressed'), 'true');
  await page.locator('#arrange-cancel').click();

  await page.evaluate(() => { projectionStats.geometry.warpAvailable = false; renderWarpEditor(); });
  assert.equal(await page.locator('#mode-simple').getAttribute('aria-pressed'), 'true');
  assert.equal(await page.locator('#mode-warp').isDisabled(), true);
  assert.equal((await read()).projection.mode, 'warp');
  await page.locator('#pj-save').click(); await page.waitForFunction(() => !configBusy);
  assert.equal(current.committed, true);
  const savedSliceX = (await read()).outputs[1].geometry.slice.x;
  await page.locator('#slice-x').fill('5'); await page.locator('#slice-x').press('Tab'); await settle();
  await page.locator('#pj-cancel').click(); await page.waitForFunction(() => !configBusy);
  // Cancel must restore the exact saved value, not merely land on something
  // other than the discarded edit.
  assert.equal((await read()).outputs[1].geometry.slice.x, savedSliceX);
  assert.deepEqual(failures, []);
  if (process.env.UI_EVIDENCE) {
    fs.mkdirSync(process.env.UI_EVIDENCE, { recursive: true });
    await page.locator('#tab-displays').screenshot({ path: `${process.env.UI_EVIDENCE}/editor.png`, style: 'header, #toast { visibility: hidden !important; }' });
    fs.writeFileSync(`${process.env.UI_EVIDENCE}/browser.json`, JSON.stringify({ recommendationCalls, maxFlight, failures }, null, 2) + '\n');
  }
  await browser.close();
})().catch(error => { console.error(error); process.exit(1); });
