// Real Chromium checks for the standalone source-browser fixture.
// Run from the repository root:
// NODE_PATH=/tmp/suede-packet5-browser/node_modules \
// PLAYWRIGHT_BROWSERS_PATH=/tmp/suede-packet5-browser/browsers \
// node tests/ui/warp-source.cjs
const assert = require('node:assert/strict');
const path = require('node:path');
const { chromium } = require('playwright');

const sourceUrl = `file://${path.resolve(__dirname, 'warp-source.html')}`;
const wait = ms => new Promise(resolve => setTimeout(resolve, ms));

(async () => {
  const browser = await chromium.launch({ headless: true });
  const context = await browser.newContext({ viewport: { width: 1280, height: 720 }, deviceScaleFactor: 2 });
  const page = await context.newPage();
  const failures = [];
  page.on('pageerror', error => failures.push(error.message));

  try {
    await page.goto(`${sourceUrl}?mode=grid&hud=0`);
    await page.waitForFunction(() => window.warpSource?.getProbe()?.canvas?.cssWidth > 0);

    const initial = await page.evaluate(() => window.warpSource.getProbe());
    assert.equal(initial.window.innerWidth, 1280);
    assert.equal(initial.window.innerHeight, 720);
    assert.equal(initial.window.devicePixelRatio, 2);
    assert.equal(initial.responsiveLayout, 'wide');
    assert.equal(initial.canvas.cssWidth, 1280);
    assert.equal(initial.canvas.cssHeight, 720);
    assert.equal(initial.canvas.backingWidth, 2560);
    assert.equal(initial.canvas.backingHeight, 1440);
    assert.equal(initial.webgl.available, true, 'Chromium fixture must expose WebGL');
    assert.equal(initial.webgl.drawingBufferWidth, initial.canvas.backingWidth);
    assert.equal(initial.webgl.drawingBufferHeight, initial.canvas.backingHeight);
    console.log('PASS initial window, DPR, canvas, and WebGL probe');

    const staticFrames = {};
    for (const name of ['dark', 'bright', 'grid']) {
      await page.evaluate(value => window.warpSource.setMode(value), name);
      const first = await page.screenshot();
      await wait(100);
      const second = await page.screenshot();
      assert(first.equals(second), `${name} mode changed pixels while static`);
      staticFrames[name] = first.toString('base64');
    }
    assert.notEqual(staticFrames.dark, staticFrames.bright, 'dark and bright modes should differ');
    assert.notEqual(staticFrames.dark, staticFrames.grid, 'dark and grid modes should differ');
    console.log('PASS dark, bright, and grid modes remain static and visually distinct');

    await page.evaluate(() => window.warpSource.setMode('moving'));
    const movingFirst = await page.screenshot();
    await wait(180);
    const movingSecond = await page.screenshot();
    assert(!movingFirst.equals(movingSecond), 'moving mode did not change pixels');
    console.log('PASS moving mode changes pixels');

    await page.setViewportSize({ width: 640, height: 480 });
    await page.waitForFunction(() => {
      const probe = window.warpSource.getProbe();
      return probe.window.innerWidth === 640 && probe.window.innerHeight === 480 && probe.responsiveLayout === 'narrow';
    });
    const resized = await page.evaluate(() => window.warpSource.getProbe());
    assert.equal(resized.canvas.cssWidth, 640);
    assert.equal(resized.canvas.cssHeight, 480);
    assert.equal(resized.canvas.backingWidth, 1280);
    assert.equal(resized.canvas.backingHeight, 960);
    assert.equal(resized.webgl.drawingBufferWidth, 1280);
    assert.equal(resized.webgl.drawingBufferHeight, 960);
    assert(await page.evaluate(() => window.warpSource.getSamples().some(sample => sample.eventType === 'resize')));
    console.log('PASS viewport resize updates dimensions, responsive layout, and event samples');

    await page.evaluate(() => window.warpSource.clearSamples());
    await page.evaluate(() => { for (let i = 0; i < 2050; i++) window.warpSource.sample(`sample-${i}`); });
    const samples = await page.evaluate(() => window.warpSource.getSamples());
    assert.equal(samples.length, 2000);
    assert.equal(samples.at(-1).reason, 'sample-2049');
    assert.equal(samples.at(-1).sampleCount, 2000);
    assert(samples.every(sample => sample.timestamp && Number.isFinite(sample.monotonicMs)));
    console.log('PASS timestamped sample history remains bounded at 2,000 entries');

    assert.deepEqual(failures, []);
    console.log(JSON.stringify({ browser: browser.version(), samples: samples.length, webgl: resized.webgl }, null, 2));
  } finally {
    await browser.close();
  }
})().catch(error => {
  console.error(error.stack || error);
  process.exitCode = 1;
});
