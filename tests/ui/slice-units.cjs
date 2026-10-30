// Browser checks for the "Slice from canvas" panel and the slice units: both
// units shown from fixtures, the Percent/Pixels radio writing the flipped unit
// and adopting the daemon's converted reply, Exact hiding the radio, dragging
// storing whole pixels in each unit, a pixels-mode preset leaving the slices
// with the hatching and the daemon's message, and hatching for a hand-dragged
// overhang. Run from the repository root; see README.md in this directory. The
// fixture intercepts every request to suede.test and never connects to an
// appliance.
const assert = require('node:assert/strict');
const fs = require('node:fs');
const { chromium } = require('playwright');

const html = fs.readFileSync('src/api/ui/index.html', 'utf8');

// Four 1920x1080 outputs in a row on an 8000x1080 canvas, as fractions.
function percentDocument() {
  const fixture = JSON.parse(fs.readFileSync('docs/examples/four-output-warp.json', 'utf8'));
  for (const output of fixture.outputs) {
    if (output.geometry && Object.hasOwn(output.geometry, 'source')) {
      output.geometry.slice = output.geometry.source;
      delete output.geometry.source;
    }
  }
  delete fixture.projection.sliceUnits;
  fixture.projection.mode = 'simple';
  fixture.projection.canvas = { aspect: 8000 / 1080, renderWidth: 8000 };
  fixture.outputs.forEach((output, i) => {
    output.geometry.slice = { x: i * 1920 / 8000, y: 0, width: 1920 / 8000, height: 1080 / 8000 };
    output.geometry.rasterFootprint = structuredClone(output.geometry.slice);
  });
  return fixture;
}
function pixelsDocument() {
  const fixture = percentDocument();
  fixture.projection.sliceUnits = 'pixels';
  fixture.outputs.forEach((output, i) => {
    output.geometry.slice = { x: i * 1920, y: 0, width: 1920, height: 1080 };
    output.geometry.rasterFootprint = structuredClone(output.geometry.slice);
  });
  return fixture;
}

// The daemon's rules, as far as this page can see them.
const unitOf = document => document.projection?.sliceUnits ?? 'percent';
const densities = canvas => {
  const height = Math.max(1, Math.round(canvas.renderWidth / canvas.aspect));
  return [canvas.renderWidth, canvas.aspect * height, height];
};
const edge = value => Math.floor(value + 0.5);
function convert(document, to) {
  const next = structuredClone(document);
  const [x, y] = densities(next.projection.canvas);
  for (const output of next.outputs) {
    const s = output.geometry.slice;
    const converted = to === 'pixels'
      ? { x: edge(s.x * x), y: edge(s.y * y), width: edge((s.x + s.width) * x) - edge(s.x * x),
          height: edge((s.y + s.height) * y) - edge(s.y * y) }
      : { x: s.x / x, y: s.y / y, width: s.width / x, height: s.height / y };
    output.geometry.slice = converted;
    output.geometry.rasterFootprint = structuredClone(converted);
  }
  if (to === 'pixels') next.projection.sliceUnits = 'pixels'; else delete next.projection.sliceUnits;
  return next;
}
function outOfCanvas(document) {
  const [x, y, height] = densities(document.projection.canvas);
  const width = document.projection.canvas.renderWidth, units = unitOf(document);
  const found = [];
  for (const output of document.outputs) {
    const s = output.geometry.slice;
    const kx = units === 'pixels' ? 1 : x, ky = units === 'pixels' ? 1 : y;
    const left = edge(s.x * kx), top = edge(s.y * ky), right = edge((s.x + s.width) * kx), bottom = edge((s.y + s.height) * ky);
    const parts = [];
    if (right > width) parts.push(`${right - width} px past the canvas's right edge`);
    if (bottom > height) parts.push(`${bottom - height} px past the canvas's bottom edge`);
    if (left < 0) parts.push(`${-left} px past the canvas's left edge`);
    if (top < 0) parts.push(`${-top} px past the canvas's top edge`);
    if (parts.length) found.push({ output: output.match.name, left: Math.max(0, -left), top: Math.max(0, -top),
      right: Math.max(0, right - width), bottom: Math.max(0, bottom - height),
      message: `${output.match.name} extends ${parts.join(' and ')}` });
  }
  return found;
}

(async () => {
  const browser = await chromium.launch({ headless: true });
  const failures = [];
  const puts = [];
  try {
    const context = await browser.newContext({ viewport: { width: 1280, height: 900 }, colorScheme: 'light' });
    const page = await context.newPage();
    page.on('pageerror', error => failures.push(error.message));
    await page.addInitScript(() => localStorage.setItem('suede.feeNoticeAcknowledged', '1'));
    let current = percentDocument();
    const names = current.outputs.map(o => o.match.name);
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
      const headers = { 'x-config-generation': '0', 'x-config-epoch': 'units-fixture', etag: '"0"' };
      if (path === '/config' && request.method() === 'PUT') {
        const body = request.postDataJSON();
        puts.push(body);
        // A flipped unit converts the slices; otherwise the document is kept.
        current = unitOf(body) !== unitOf(current) ? convert(body, unitOf(body)) : structuredClone(body);
        if (unitOf(body) !== unitOf(current)) throw new Error('unit mismatch');
        return respond(current, headers);
      }
      if (path === '/config') return respond(current, headers);
      if (path === '/projection/stats') return respond({
        running: true, lastInterval: null,
        geometry: { warpAvailable: true, effectiveMode: current.projection.mode, exactAvailable: true,
          outOfCanvas: outOfCanvas(current) },
        control: { session: 's', outputs: [] },
      });
      if (path === '/projection/recommendation') return respond({
        idealWidth: 8000, requestedAspect: 8000 / 1080, presets: [], warnings: [], approximate: false,
        generation: 0, revision: 0 }, headers);
      if (path === '/av') return respond({ audioOutputs: [], audioInputs: [], videoInputs: [] });
      if (path === '/system') return respond({ suedeVersion: 'test', uptimeSeconds: 1, packages: [] });
      if (path === '/status') return respond({ state: 'synced', divergences: [] });
      if (path === '/system/gpu') return respond({ available: true, gpus: [], processes: [] });
      if (path === '/system/cpu') return respond({});
      if (path === '/system/pipeline') return respond(null);
      return respond([]);
    });
    const settle = () => page.evaluate(() => flushPreviews());
    const visible = id => page.locator(`#${id}`).isVisible();
    const refreshStats = () => page.evaluate(async () => { projectionStats = await api('/projection/stats'); renderWarpStatus(); renderLayout(); });
    const load = async document => {
      current = document;
      await page.evaluate(async () => { await loadConfig(); });
      await refreshStats();
      await page.evaluate(() => selectOutput(config.outputs[1].match.name));
    };
    const field = id => page.locator(`#${id}`).inputValue();
    const stored = index => page.evaluate(i => structuredClone(config.outputs[i].geometry.slice), index);
    const overhangs = () => page.locator('#layout .display-box .overhang').count();
    const drag = async (index, dx, dy) => {
      const name = names[index];
      await page.locator(`.display-box[data-name="${name}"]`).scrollIntoViewIfNeeded();
      const box = await page.locator(`.display-box[data-name="${name}"]`).boundingBox();
      const x = box.x + box.width / 2, y = box.y + box.height / 2;
      await page.mouse.move(x, y);
      await page.mouse.down();
      await page.mouse.move(x + dx, y + dy, { steps: 5 });
      await page.mouse.up();
      await settle();
    };

    await page.goto('http://suede.test/?nosse');
    await page.waitForFunction(() => config && configGeneration === '0');
    await load(percentDocument());

    // Percent: the stored fractions are shown as percentages, and drawn as before.
    assert.equal(await page.locator('legend', { hasText: 'Slice from canvas' }).count(), 1);
    assert.equal(await page.locator('legend', { hasText: 'Shared content selection' }).count(), 0);
    assert(await visible('slice-units-row'));
    assert(await page.locator('#slice-units-percent').isChecked());
    assert.equal(await field('slice-x'), '24');
    assert.equal(await field('slice-y'), '0');
    assert.equal(await field('slice-width'), '24');
    assert.equal(await field('slice-height'), '13.5');
    assert.equal(await field('content-scale'), '100');
    assert.equal(await page.locator('#slice-x').getAttribute('step'), 'any');
    assert.match(await page.locator('#source-size').textContent(), /Crop 1920 × 1080 px; scale 100% · 1:1/);
    assert.match(await page.locator('#slice-explain').textContent(), /^Simple /);
    assert.equal(await overhangs(), 0, 'a percent fixture inside the canvas has no hatching');
    const widths = await page.locator('#layout .display-box').evaluateAll(boxes => boxes.map(b => b.getBoundingClientRect().width));
    const frame = await page.locator('#layout .canvas-frame').evaluate(f => f.getBoundingClientRect().width);
    for (const width of widths) assert(Math.abs(width / frame - 0.24) < 0.01, `box ${width} of frame ${frame}`);
    assert.match(await page.locator('.display-box[data-name="' + names[1] + '"]').textContent(), /1920×1080/);

    // A content scale edit stores a fraction.
    await page.locator('#content-scale').fill('50');
    await page.locator('#content-scale').dispatchEvent('change');
    await settle();
    let s = await stored(1);
    assert(Math.abs(s.width - 3840 / 8000) < 1e-12 && Math.abs(s.height - 2160 / 8000) < 1e-12);
    await page.locator('#content-scale').blur();
    await page.evaluate(() => renderSharedContent());
    assert.match(await page.locator('#source-size').textContent(), /scale 50% · 1:2/);
    await page.locator('#content-scale').fill('100');
    await page.locator('#content-scale').dispatchEvent('change');
    await settle();
    await page.locator('#content-scale').blur();
    await page.evaluate(() => renderSharedContent());
    // A percent width is stored as a fraction.
    await page.locator('#slice-width').fill('12');
    await page.locator('#slice-width').dispatchEvent('change');
    await settle();
    assert.equal((await stored(1)).width, 0.12);
    await page.locator('#slice-width').fill('24');
    await page.locator('#slice-width').dispatchEvent('change');
    await settle();

    // Dragging in percent moves whole pixels and stores px / density.
    await drag(1, 40, 0);
    s = await stored(1);
    const px = s.x * 8000;
    assert(Math.abs(px - Math.round(px)) < 1e-9 && Math.round(px) !== 1920, `percent drag stored ${px} px`);
    assert.equal(s.width, 0.24, 'the size is left alone');
    assert.equal(puts.at(-1).projection.sliceUnits, 'percent', 'the unit is always sent');
    await page.locator('#slice-x').fill('24');
    await page.locator('#slice-x').dispatchEvent('change');
    await settle();

    // Toggling: the flipped unit is sent with the slices as they are, and the
    // daemon's converted reply is adopted.
    const before = puts.length;
    await page.locator('#slice-units-pixels').check();
    await page.waitForFunction(() => config.projection.sliceUnits === 'pixels');
    await settle();
    const flip = puts.at(before);
    assert.equal(flip.projection.sliceUnits, 'pixels');
    assert.equal(flip.outputs[1].geometry.slice.x, 0.24, 'slices unchanged in the write');
    assert.deepEqual(await stored(1), { x: 1920, y: 0, width: 1920, height: 1080 }, 'adopted the converted reply');
    assert(await page.locator('#slice-units-pixels').isChecked());
    assert.equal(await field('slice-x'), '1920');
    assert.equal(await field('slice-width'), '1920');
    assert.equal(await field('slice-height'), '1080');
    assert.equal(await page.locator('#slice-x').getAttribute('step'), '1');

    // Pixels: integers, a drag stores whole pixels, a fraction is rounded.
    await drag(1, 40, 0);
    s = await stored(1);
    assert(Number.isInteger(s.x) && Number.isInteger(s.y) && s.x !== 1920, `pixels drag stored ${s.x}`);
    assert.equal(puts.at(-1).projection.sliceUnits, 'pixels');
    await page.locator('#slice-x').fill('1930.6');
    await page.locator('#slice-x').dispatchEvent('change');
    await settle();
    assert.equal(await field('slice-x'), '1931');
    assert.equal((await stored(1)).x, 1931);
    await page.locator('#slice-x').fill('1920');
    await page.locator('#slice-x').dispatchEvent('change');
    await settle();
    await page.locator('#content-scale').fill('90');
    await page.locator('#content-scale').dispatchEvent('change');
    await settle();
    s = await stored(1);
    assert(Number.isInteger(s.width) && Number.isInteger(s.height), 'a scale edit stores whole pixels');
    assert.equal(s.width, 2133);
    await page.locator('#content-scale').fill('100');
    await page.locator('#content-scale').dispatchEvent('change');
    await settle();

    // Toggling back converts exactly.
    await page.locator('#slice-units-percent').check();
    await page.waitForFunction(() => !config.projection.sliceUnits || config.projection.sliceUnits === 'percent');
    await settle();
    assert.equal(puts.at(-1).projection.sliceUnits, 'percent');
    assert.equal(puts.at(-1).outputs[1].geometry.slice.x, 1920, 'pixels sent unchanged');
    assert.equal((await stored(1)).x, 0.24);
    assert.equal(await field('slice-x'), '24');

    // A pixels preset (a render-width change) leaves the slices alone, hatches
    // the part past the canvas and shows the daemon's message.
    await load(pixelsDocument());
    assert.equal(await overhangs(), 0);
    await page.evaluate(() => setCanvasWidth(4000));
    await settle();
    assert.deepEqual(await stored(3), { x: 5760, y: 0, width: 1920, height: 1080 }, 'slices untouched by the preset');
    assert.deepEqual(await stored(1), { x: 1920, y: 0, width: 1920, height: 1080 });
    assert.equal(puts.at(-1).projection.canvas.renderWidth, 4000);
    await refreshStats();
    assert(await overhangs() > 0, 'the overhang is hatched');
    const marked = await page.locator('#layout .display-box[data-overhang]').evaluateAll(b => b.map(x => x.dataset.name));
    // At 4000 x 540 every 1920 x 1080 slice hangs past the bottom, and the last two past the right.
    assert.deepEqual(marked, names, 'every display is flagged');
    assert.match(await page.locator(`.display-box[data-name="${names[0]}"]`).getAttribute('data-overhang'), /^bottom:540$/);
    assert.match(await page.locator(`.display-box[data-name="${names[3]}"]`).getAttribute('data-overhang'), /^right:\d+ bottom:540$/);
    const status = await page.locator('#warp-status').textContent();
    assert(status.includes(`${names[3]} extends`) && status.includes("past the canvas's right edge"), status);
    await page.evaluate(() => setCanvasWidth(8000));
    await settle();
    await refreshStats();
    assert.equal(await overhangs(), 0, 'back inside the canvas: no hatching');

    // Hatching for a hand-dragged overhang, shown at once, in each unit.
    for (const [label, document] of [['pixels', pixelsDocument()], ['percent', percentDocument()]]) {
      await load(document);
      await page.evaluate(i => selectOutput(config.outputs[i].match.name), 3);
      assert.equal(await overhangs(), 0, `${label}: nothing yet`);
      const name = names[3];
      await page.locator(`.display-box[data-name="${name}"]`).scrollIntoViewIfNeeded();
      const box = await page.locator(`.display-box[data-name="${name}"]`).boundingBox();
      await page.mouse.move(box.x + box.width / 2, box.y + box.height / 2);
      await page.mouse.down();
      await page.mouse.move(box.x + box.width / 2 + 60, box.y + box.height / 2, { steps: 5 });
      assert(await page.locator(`.display-box[data-name="${name}"] .overhang-right`).count() === 1, `${label}: hatched during the drag`);
      await page.mouse.up();
      await settle();
      assert.equal(await page.locator(`.display-box[data-name="${name}"] .overhang-right`).count(), 1, `${label}: hatched after the drop`);
      const right = await page.evaluate(i => { const p = sliceToPx(config.outputs[i].geometry.slice); return p.x + p.width - canvasPixels().width; }, 3);
      assert(right > 0, `${label}: the slice hangs ${right} px past the canvas`);
      assert.equal(await page.locator(`.display-box[data-name="${name}"]`).getAttribute('data-overhang'), `right:${right}`);
      assert.equal(await page.locator(`.display-box[data-name="${names[0]}"] .overhang`).count(), 0);
    }

    // Exact has no unit to choose and no editable size.
    const exact = pixelsDocument();
    exact.projection.mode = 'exact';
    await load(exact);
    assert(!(await visible('slice-units-row')), 'the radio is hidden in Exact');
    assert(!(await visible('content-scale')));
    assert.equal(await field('slice-x'), '1920');
    assert(await page.locator('#slice-width').evaluate(e => e.readOnly));
    assert.match(await page.locator('#slice-explain').textContent(), /^Exact /);
    assert.match(await page.locator('#source-size').textContent(), /Crop 1920 × 1080 px.*scale 100% · 1:1/);
  } catch (error) {
    failures.push(error.stack || String(error));
  } finally {
    await browser.close();
  }
  if (failures.length) { console.error(failures.join('\n')); process.exit(1); }
  console.log('slice units UI checks passed');
})();
