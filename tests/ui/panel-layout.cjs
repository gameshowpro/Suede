// Browser acceptance checks for the responsive independent panel stacks.
// Run from the repository root with Playwright and Chromium installed outside
// the repository; see README.md in this directory.
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
const waitForLayout = page => page.waitForFunction(() =>
  [...document.querySelectorAll('.panel-grid')].some(grid => grid.classList.contains('masonry')));
const waitForColumns = (page, selector, columns) => page.waitForFunction(({ selector, columns }) => {
  const grid = document.querySelector(selector);
  if (!grid || getComputedStyle(grid).getPropertyValue('--panel-columns').trim() !== String(columns)) return false;
  let index = 0;
  const visible = [...grid.children].filter(item => item.matches('.panel, .alert, #warp-error') && item.getClientRects().length
    && item.getBoundingClientRect().height > 0);
  return visible.every(item => {
    const style = getComputedStyle(item);
    const span = item.id === 'warp-error' || style.getPropertyValue('--panel-span').trim() === 'all';
    const preferred = Number(style.getPropertyValue('--panel-column'));
    const column = Math.min(columns - 1, preferred ? preferred - 1 : index++ % columns);
    return item.style.gridColumn === (span ? '1 / -1' : String(column + 1)) && !!item.style.gridRow;
  });
}, { selector, columns });
const metrics = (page, selector) => page.locator(selector).evaluateAll(nodes => nodes.map(node => {
  const rect = node.getBoundingClientRect();
  return { id: node.id, x: rect.x, y: rect.y, width: rect.width, height: rect.height,
    right: rect.right, bottom: rect.bottom, display: getComputedStyle(node).display,
    column: node.style.gridColumn, row: node.style.gridRow };
}));
const closeTo = (actual, expected, tolerance = 1, message) =>
  assert(Math.abs(actual - expected) <= tolerance, message || `${actual} should be within ${tolerance}px of ${expected}px`);
const mainLeftColumnX = page => page.locator('main').evaluate(main => main.getBoundingClientRect().left + 20);
const assertColumn = (cards, id, x, message) => closeTo(cards.find(card => card.id === id).x, x, 1, message);
const verifyGrid = async (page, selector, columns) => {
  const cards = (await metrics(page, `${selector} > .panel, ${selector} > .alert`))
    .filter(card => card.width > 0);
  assert(cards.length > 0, `${selector} has visible cards`);
  const distinctX = [...new Set(cards.map(card => Math.round(card.x)))].sort((a, b) => a - b);
  assert.equal(distinctX.length, columns, `${selector} has ${columns} visible column(s): ${JSON.stringify(cards)}`);
  for (const card of cards) assert(card.width <= 900, `${selector} ${card.id} width ${card.width} exceeds 900px`);
  if (columns === 2) closeTo(cards[0].width, cards.find(card => Math.round(card.x) !== Math.round(cards[0].x)).width, 1);
  for (const x of distinctX) {
    const stack = cards.filter(card => Math.round(card.x) === x).sort((a, b) => a.y - b.y);
    for (let index = 1; index < stack.length; index++) {
      closeTo(stack[index].y - stack[index - 1].bottom, 16, 1);
    }
  }
  return cards;
};

(async () => {
  const browser = await chromium.launch({ headless: true });
  const failures = [];
  try {
    const context = await browser.newContext({ viewport: { width: 1280, height: 900 }, colorScheme: 'light' });
    const page = await context.newPage();
    page.on('pageerror', error => failures.push(error.message));
    await page.route('**/*', async route => {
      const url = new URL(route.request().url());
      const respond = (body, headers = {}) => route.fulfill({ status: 200, contentType: 'application/json',
        headers, body: JSON.stringify(body) });
      if (url.pathname === '/') return route.fulfill({ contentType: 'text/html', body: html });
      if (url.pathname === '/assets/fonts/fonts.css') return route.fulfill({
        contentType: 'text/css', body: fs.readFileSync('fonts/fonts.css'),
      });
      if (url.pathname.startsWith('/assets/fonts/')) {
        const name = url.pathname.slice('/assets/fonts/'.length);
        if (name.endsWith('.woff2')) return route.fulfill({
          contentType: 'font/woff2', body: fs.readFileSync(`fonts/${name}`),
        });
      }
      const path = url.pathname.replace('/api/v1', '');
      if (path === '/config') return respond(fixture, {
        'x-config-generation': '0', 'x-config-epoch': 'layout-fixture', etag: `"${fixture.revision ?? 0}"`,
      });
      if (path === '/av') return respond({ audioOutputs: [], audioInputs: [], videoInputs: [] });
      if (path === '/system') return respond({ suedeVersion: 'test', uptimeSeconds: 1, packages: [] });
      if (path === '/status') return respond({ state: 'synced', divergences: [] });
      if (path === '/system/checks') return respond([]);
      if (path === '/system/gpu') return respond({ available: true, gpus: [], processes: [] });
      if (path === '/system/cpu') return respond({});
      if (path === '/system/browsers') return respond([]);
      if (path === '/projection/stats' || path === '/system/pipeline') return respond(null);
      return respond([]);
    });

    await page.goto('http://suede.test/?nosse');
    await page.waitForFunction(() => configGeneration === '0' && document.querySelector('#tab-config'));
    await waitForLayout(page);

    // The initially hidden config tab must be measured only after activation.
    await page.locator('nav button[data-tab="config"]').click();
    await page.evaluate(() => {
      const grid = document.querySelector('#tab-config');
      grid.replaceChildren();
      const make = (id, height) => {
        const panel = document.createElement('div');
        panel.className = 'panel'; panel.id = id;
        panel.style.height = `${height}px`;
        panel.innerHTML = `<button type="button">${id}</button>`;
        return panel;
      };
      grid.append(make('layout-fixture-a', 120), make('layout-fixture-b', 80),
        make('layout-fixture-c', 150), make('layout-fixture-d', 90));
      const dialog = document.createElement('dialog'); dialog.id = 'layout-fixture-dialog';
      dialog.textContent = 'not a panel'; grid.append(dialog);
    });
    await page.waitForFunction(() => document.querySelector('#layout-fixture-a')?.style.gridRow);
    await verifyGrid(page, '#tab-config', 1);
    assert.equal((await metrics(page, '#layout-fixture-dialog')).length, 1);
    assert.equal((await metrics(page, '#layout-fixture-dialog'))[0].width, 0, 'closed dialog is outside layout');

    // At the 1856px breakpoint the measured stacks are equal-width and capped.
    await page.setViewportSize({ width: 1855, height: 1000 });
    await waitForColumns(page, '#tab-config', 1);
    await verifyGrid(page, '#tab-config', 1);
    await page.setViewportSize({ width: 1856, height: 1000 });
    await waitForColumns(page, '#tab-config', 2);
    let cards = await verifyGrid(page, '#tab-config', 2);
    const a = cards.find(card => card.id === 'layout-fixture-a');
    const b = cards.find(card => card.id === 'layout-fixture-b');
    const c = cards.find(card => card.id === 'layout-fixture-c');
    assert.notEqual(Math.round(a.x), Math.round(b.x), 'round robin starts the next panel in the other column');
    assert.equal(Math.round(a.x), Math.round(c.x), 'later panels keep their stable round-robin column');

    // Apps uses both columns together only at the wide breakpoint.
    await page.locator('nav button[data-tab="apps"]').click();
    await waitForColumns(page, '#tab-apps', 2);
    assert.equal(await page.locator('#tab-apps > .panel').count(), 1, 'Apps has one card');
    let appCard = (await metrics(page, '#tab-apps > .panel'))[0];
    closeTo(appCard.width, 1816, 1);
    closeTo(appCard.x, await mainLeftColumnX(page), 1);
    await page.setViewportSize({ width: 1855, height: 1000 });
    await waitForColumns(page, '#tab-apps', 1);
    appCard = (await metrics(page, '#tab-apps > .panel'))[0];
    assert(appCard.width <= 900, `Apps card is capped below the two-column breakpoint: ${appCard.width}px`);
    await page.setViewportSize({ width: 1856, height: 1000 });
    await waitForColumns(page, '#tab-apps', 2);
    appCard = (await metrics(page, '#tab-apps > .panel'))[0];
    closeTo(appCard.width, 1816, 1);
    await page.locator('nav button[data-tab="config"]').click();
    await waitForColumns(page, '#tab-config', 2);

    // A resize-driven height change must preserve focus and leave the other
    // column's stack in place while maintaining the same-column gap.
    await page.locator('#layout-fixture-a button').focus();
    await page.evaluate(() => {
      const panel = document.querySelector('#layout-fixture-a');
      panel.style.height = 'auto';
      const content = document.createElement('div'); content.style.height = '220px';
      panel.append(content);
    });
    await page.waitForFunction(() => document.querySelector('#layout-fixture-a').getBoundingClientRect().height > 260);
    await page.waitForTimeout(60);
    cards = await verifyGrid(page, '#tab-config', 2);
    const aAfter = cards.find(card => card.id === 'layout-fixture-a');
    const bAfter = cards.find(card => card.id === 'layout-fixture-b');
    assert.equal(await page.evaluate(() => document.activeElement.textContent), 'layout-fixture-a', 'height updates preserve focus');
    closeTo(bAfter.x, b.x, 0); closeTo(bAfter.y, b.y, 0);
    closeTo(cards.find(card => card.id === 'layout-fixture-c').y - aAfter.bottom, 16, 1);

    // Hidden and removed cards leave no phantom stack space; replacement
    // alerts remain in their own independently measured grid.
    await page.evaluate(() => document.querySelector('#layout-fixture-b').classList.add('hidden'));
    await page.waitForTimeout(60);
    assert.equal((await metrics(page, '#layout-fixture-b'))[0].width, 0);
    await page.evaluate(() => document.querySelector('#layout-fixture-d').remove());
    await page.waitForTimeout(60);
    await verifyGrid(page, '#tab-config', 2);
    await page.evaluate(() => {
      const alerts = document.querySelector('#alerts');
      alerts.replaceChildren(...['warning-one', 'error-two', 'warning-three'].map((id, i) => {
        const alert = document.createElement('div'); alert.className = `alert ${i === 1 ? 'fail' : 'warn'}`;
        alert.id = id; alert.style.height = `${70 + i * 10}px`; alert.textContent = id; return alert;
      }));
    });
    await page.waitForFunction(() => document.querySelector('#warning-one')?.style.gridRow);
    await verifyGrid(page, '#alerts', 2);
    await page.evaluate(() => document.querySelector('#alerts').replaceChildren());
    await page.waitForTimeout(60);
    assert.equal((await metrics(page, '#alerts > .alert')).length, 0, 'replaced alert group clears old cards');

    // Width cap and single-column behavior at compact and very wide viewports,
    // in both color schemes. Color scheme must not affect measurements.
    await page.setViewportSize({ width: 320, height: 800 });
    await waitForColumns(page, '#tab-config', 1);
    await verifyGrid(page, '#tab-config', 1);
    await page.emulateMedia({ colorScheme: 'dark' });
    await verifyGrid(page, '#tab-config', 1);
    await page.setViewportSize({ width: 2560, height: 1000 });
    await waitForColumns(page, '#tab-config', 2);
    await verifyGrid(page, '#tab-config', 2);
    await page.emulateMedia({ colorScheme: 'light' });
    await verifyGrid(page, '#tab-config', 2);

    // Displays keep the related panels aligned in their intended columns.
    await page.locator('nav button[data-tab="displays"]').click();
    await waitForColumns(page, '#tab-displays', 2);
    let displayCards = await metrics(page, '#tab-displays > .panel');
    const leftX = await mainLeftColumnX(page);
    assertColumn(displayCards, 'layout-panel', leftX, 'Layout stays in the left column');
    assertColumn(displayCards, 'edge-blending-panel', leftX + 916, 'Edge blending stays in the right column');
    assertColumn(displayCards, 'frame-timing-panel', leftX + 916, 'Frame timing stays in the right column');
    closeTo(displayCards.find(card => card.id === 'frame-timing-panel').y
      - displayCards.find(card => card.id === 'edge-blending-panel').bottom, 16, 1);
    assert.equal(await page.locator('#tab-displays > dialog').count(), 1);
    assert.equal(await page.locator('#tab-displays > dialog').evaluate(el => el.style.gridRow), '');
    await page.locator('#output-table tbody tr').first().click();
    await page.waitForFunction(() => document.querySelector('#output-editor').getClientRects().length > 0);
    await waitForColumns(page, '#tab-displays', 2);
    displayCards = await metrics(page, '#tab-displays > .panel');
    assertColumn(displayCards, 'output-editor', leftX, 'Selected output editor remains below Layout in the left column');
    closeTo(displayCards.find(card => card.id === 'output-editor').y
      - displayCards.find(card => card.id === 'layout-panel').bottom, 16, 1);
    await page.setViewportSize({ width: 1855, height: 1000 });
    await waitForColumns(page, '#tab-displays', 1);
    displayCards = await metrics(page, '#tab-displays > .panel');
    const narrowDisplays = displayCards.filter(card => card.width > 0).sort((a, b) => a.y - b.y);
    for (const card of narrowDisplays) closeTo(card.x, narrowDisplays[0].x, 1);
    for (let index = 1; index < narrowDisplays.length; index++) {
      closeTo(narrowDisplays[index].y - narrowDisplays[index - 1].bottom, 16, 1);
    }
    await page.setViewportSize({ width: 2560, height: 1000 });
    await waitForColumns(page, '#tab-displays', 2);

    // System telemetry prefers the right stack, while every other card stays
    // left even after those telemetry panels become hidden and visible again.
    await page.locator('nav button[data-tab="system"]').click();
    await waitForColumns(page, '#tab-system', 2);
    let systemCards = await metrics(page, '#tab-system > .panel');
    assertColumn(systemCards, 'gpu-panel', leftX + 916, 'GPU is in the right column');
    assertColumn(systemCards, 'cpu-panel', leftX + 916, 'CPU is in the right column');
    const systemLeft = systemCards.filter(card => card.width > 0 && card.id !== 'gpu-panel' && card.id !== 'cpu-panel');
    assert.equal(systemLeft.length, 2, 'only System and Health checks remain visible');
    for (const card of systemLeft) {
      closeTo(card.x, leftX, 1);
      assert(card.width <= 900, `${card.id} width ${card.width} exceeds 900px`);
    }
    closeTo(systemCards.find(card => card.id === 'cpu-panel').y
      - systemCards.find(card => card.id === 'gpu-panel').bottom, 16, 1);
    assert.equal(await page.locator('#tab-system > dialog').count(), 1);
    assert.equal(await page.locator('#tab-system > dialog').evaluate(el => el.style.gridRow), '');
    // The less common pipeline and Chromium panels use the left stack too.
    await page.evaluate(() => {
      document.querySelector('#pipeline-panel').hidden = false;
      document.querySelector('#browsers-panel').hidden = false;
    });
    await waitForColumns(page, '#tab-system', 2);
    systemCards = await metrics(page, '#tab-system > .panel');
    assertColumn(systemCards, 'pipeline-panel', leftX, 'Pipeline belongs in the left column');
    assertColumn(systemCards, 'browsers-panel', leftX, 'Chromium belongs in the left column');
    await page.evaluate(() => {
      gpuTelemetry = null; cpuTelemetry = null; renderGpuAndBrowsers();
    });
    await page.waitForFunction(() => document.querySelector('#gpu-panel').hidden && document.querySelector('#cpu-panel').hidden);
    await waitForColumns(page, '#tab-system', 2);
    systemCards = await metrics(page, '#tab-system > .panel');
    const remainingSystemCards = systemCards.filter(card => card.width > 0);
    assert.equal(remainingSystemCards.length, 2);
    for (const card of remainingSystemCards) {
      closeTo(card.x, leftX, 1);
      assert(card.width <= 900, `${card.id} width ${card.width} exceeds 900px`);
    }
    await page.evaluate(() => {
      gpuTelemetry = { available: true, gpus: [], processes: [] };
      cpuTelemetry = {}; renderGpuAndBrowsers();
    });
    await page.waitForFunction(() => !document.querySelector('#gpu-panel').hidden && !document.querySelector('#cpu-panel').hidden);
    await waitForColumns(page, '#tab-system', 2);
    systemCards = await metrics(page, '#tab-system > .panel');
    assertColumn(systemCards, 'gpu-panel', leftX + 916, 'GPU returns to its preferred right column');
    assertColumn(systemCards, 'cpu-panel', leftX + 916, 'CPU returns to its preferred right column');
    await page.setViewportSize({ width: 1855, height: 1000 });
    await waitForColumns(page, '#tab-system', 1);
    systemCards = (await metrics(page, '#tab-system > .panel')).filter(card => card.width > 0).sort((a, b) => a.y - b.y);
    for (const card of systemCards) closeTo(card.x, systemCards[0].x, 1);
    for (let index = 1; index < systemCards.length; index++) {
      closeTo(systemCards[index].y - systemCards[index - 1].bottom, 16, 1);
    }
    assert.deepEqual(failures, []);
    console.log(`PASS panel layout acceptance (${browser.version()})`);
  } finally {
    await browser.close();
  }
})().catch(error => {
  console.error(error.stack || error);
  process.exitCode = 1;
});
