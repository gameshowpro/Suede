// Browser acceptance checks for the System Profiles tab.
// Run with the external Playwright setup documented in tests/ui/README.md.
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
const sleep = ms => new Promise(resolve => setTimeout(resolve, ms));

(async () => {
  const browser = await chromium.launch({ headless: true });
  const failures = [], requests = [];
  let current = structuredClone(fixture), committed = structuredClone(fixture);
  let profiles = [{ id: 'screen', name: 'Screen profile', description: '', includes: {
    displays: true, backgrounds: true, apps: true, activeApp: true, settings: true,
  } }];
  let exported = { formatVersion: 1, name: 'Screen profile', includes: profiles[0].includes,
    config: { ...structuredClone(fixture), outputs: [
      { match: { name: 'HDMI-A-1', make: 'Acme', model: 'Panel', serial: '42' } },
      { match: { name: 'NOT-CONNECTED' } },
    ],
      apps: [{ id: 'stage', audio: { output: 'profile-sink' } }] }, assets: {} };
  let activationStatus = 200;
  try {
    const context = await browser.newContext({ viewport: { width: 1280, height: 900 } });
    const page = await context.newPage();
    page.on('pageerror', error => failures.push(error.message));
    await page.addInitScript(() => localStorage.setItem('suede.feeNoticeAcknowledged', '1'));
    await page.route('**/*', async route => {
      const request = route.request(), url = new URL(request.url());
      const path = url.pathname.replace('/api/v1', '');
      const reply = (body, status = 200) => route.fulfill({ status, contentType: 'application/json',
        headers: { 'x-config-generation': '7', 'x-config-epoch': 'profile-fixture', etag: `"${current.revision ?? 0}"` },
        body: JSON.stringify(body) });
      if (url.pathname === '/') return route.fulfill({ contentType: 'text/html', body: html });
      if (path === '/config' && request.method() === 'GET') return reply(current);
      if (path === '/config' && request.method() === 'PUT') {
        const body = request.postDataJSON(); requests.push({ path, method: 'PUT', body });
        current = { ...body, committed: body.committed === true };
        if (current.committed) committed = structuredClone(current);
        return reply(current);
      }
      if (path === '/config/revert' && request.method() === 'POST') {
        current = structuredClone(committed); return reply(current);
      }
      if (path === '/profiles' && request.method() === 'GET') return reply(profiles);
      if (path === '/profiles' && request.method() === 'POST') {
        const body = request.postDataJSON(); requests.push({ path, method: 'POST', body });
        profiles.push({ ...body }); return reply(profiles.at(-1), 201);
      }
      if (path === '/profiles/import' && request.method() === 'POST') {
        const body = request.postDataJSON(); requests.push({ path, method: 'POST', body });
        profiles.push({ id: body.id, name: body.profile.name, description: body.profile.description || '', includes: body.profile.includes });
        exported = structuredClone(body.profile);
        return reply(profiles.at(-1), 201);
      }
      if (path.startsWith('/profiles/') && path.endsWith('/activate')) {
        const body = request.postDataJSON();
        requests.push({ path, method: 'POST', body, headers: request.headers() });
        if (activationStatus !== 200) return reply({ detail: 'stale configuration version' }, activationStatus);
        current = { ...current, committed: true }; committed = structuredClone(current);
        return reply(current);
      }
      if (path.startsWith('/profiles/') && request.method() === 'GET') return reply(exported);
      if (path === '/av') return reply({ audioOutputs: [], audioInputs: [], videoInputs: [] });
      if (path === '/system') return reply({ suedeVersion: 'test', uptimeSeconds: 1, packages: [] });
      if (path === '/status') return reply({ state: 'synced', committed: current.committed, divergences: [] });
      if (path === '/system/checks') return reply([]);
      if (path === '/system/gpu') return reply({ available: true, gpus: [], processes: [] });
      if (path === '/system/cpu') return reply({});
      if (path === '/system/browsers' || path === '/wallpapers') return reply([]);
      if (path === '/projection/stats' || path === '/system/pipeline') return reply(null);
      return reply([]);
    });

    await page.goto('http://suede.test/?nosse');
    await page.waitForFunction(() => config && configGeneration === '7' && document.querySelector('#tab-profiles'));
    await page.locator('nav button[data-tab="profiles"]').click();
    await page.locator('#profile-new').click();
    await page.locator('#profile-name').fill('Opening set');
    await page.locator('#profile-description').fill('House lights and stage');
    await page.locator('[data-profile-include="displays"]').uncheck();
    await page.locator('[data-profile-include="apps"]').uncheck();
    await page.locator('#profile-save').click();
    await page.waitForFunction(() => document.querySelector('#profile-editor').hidden);
    const created = requests.find(entry => entry.path === '/profiles' && entry.method === 'POST');
    assert(created, 'Save new sends POST /profiles');
    assert.match(created.body.id, /^[0-9a-f-]{36}$/i, 'new profile has an ID');
    assert.equal(created.body.name, 'Opening set');
    assert.equal(created.body.description, 'House lights and stage');
    assert.equal(created.body.includes.displays, false);
    assert.equal(created.body.includes.apps, false);
    assert.equal(created.body.includes.backgrounds, true);
    await page.locator('#profile-new').click();
    await page.locator('#profile-name').fill(' opening SET ');
    await page.locator('#profile-save').click();
    assert.equal(requests.filter(entry => entry.path === '/profiles' && entry.method === 'POST').length, 1,
      'creating a profile refuses an existing name after trimming and case folding');
    assert.match(await page.locator('#profile-message').textContent(), /already exists/i);
    await page.locator('#profile-editor-cancel').click();

    await page.locator('#profile-import-file').setInputFiles({ name: 'imported.json', mimeType: 'application/json',
      buffer: Buffer.from(JSON.stringify(exported)) });
    await page.locator('#profile-import-dialog').waitFor({ state: 'visible' });
    const importPosts = () => requests.filter(entry => entry.path === '/profiles/import' && entry.method === 'POST');
    assert.equal(importPosts().length, 0, 'selecting a file only opens name review and sends no import request');
    await page.locator('#profile-import-save').click();
    assert.equal(importPosts().length, 0, 'blank import name is refused');
    assert.match(await page.locator('#profile-import-error').textContent(), /Enter a new profile name/i);
    await page.locator('#profile-import-name').fill('  SCREEN PROFILE  ');
    await page.locator('#profile-import-save').click();
    assert.equal(importPosts().length, 0, 'an existing name is refused case-insensitively after trimming');
    assert.match(await page.locator('#profile-import-error').textContent(), /already exists/i);
    await page.locator('#profile-import-name').fill('Imported Screen');
    await page.locator('#profile-import-save').click();
    await page.waitForFunction(() => document.querySelector('#profile-message').textContent === 'Profile imported.');
    const imported = importPosts()[0];
    assert(imported, 'a new import name sends POST /profiles/import');
    assert.equal(imported.body.profile.name, 'Imported Screen', 'the supplied name replaces the source profile name');
    assert.deepEqual(imported.body.profile.config, exported.config, 'other imported profile content is preserved');
    assert.deepEqual(imported.body.profile.assets, exported.assets, 'imported assets are preserved');
    assert.equal(requests.filter(entry => entry.path.endsWith('/activate')).length, 0,
      'import stores the profile without activating it');

    await page.locator('#profile-table button:has-text("Duplicate")').first().click();
    await page.locator('#profile-import-dialog').waitFor({ state: 'visible' });
    assert.equal(importPosts().length, 1, 'duplicate action requires a name before POST');
    assert.equal(await page.locator('#profile-import-name').inputValue(), '', 'duplicate name is not prefilled');
    await page.locator('#profile-import-name').fill('Opening set');
    await page.locator('#profile-import-save').click();
    assert.equal(importPosts().length, 1, 'duplicate also refuses an existing name');
    assert.match(await page.locator('#profile-import-error').textContent(), /already exists/i);
    await page.locator('#profile-import-name').fill('Screen duplicate');
    await page.locator('#profile-import-save').click();
    await page.waitForFunction(() => document.querySelector('#profile-message').textContent === 'Created “Screen duplicate”.');
    assert.equal(importPosts()[1].body.profile.name, 'Screen duplicate');
    assert.deepEqual(importPosts()[1].body.profile.config, exported.config, 'duplicate preserves source content');

    // An accepted preview makes the shared working copy dirty. The tab must
    // direct the operator to Save or Revert and must not open activation.
    await page.locator('nav button[data-tab="displays"]').click();
    await page.locator('#canvas-aspect').fill('1.8');
    await page.locator('#canvas-aspect').dispatchEvent('change');
    await page.waitForFunction(() => config?.committed === false);
    await page.locator('nav button[data-tab="profiles"]').click();
    await page.locator('#profile-table button:has-text("Activate")').first().click();
    assert.equal(requests.filter(entry => entry.path.endsWith('/activate')).length, 0,
      'activation is blocked while unsaved edits exist');
    assert.match(await page.locator('#toast').textContent(), /Save or Revert/i);
    await page.locator('#pj-cancel').click();
    await page.waitForFunction(() => config?.committed !== false);

    // The profile's OutputMatch has several criteria, so its source map key
    // must retain the exact edid:name:make:model:serial API representation.
    await page.locator('#profile-table button:has-text("Activate")').first().click();
    await page.locator('[data-profile-activate]').waitFor();
    assert.equal(await page.locator('#profile-map-0').inputValue(), 'HDMI-A-1');
    assert.equal(await page.locator('#profile-map-1').inputValue(), '',
      'an unknown display is left unmapped instead of targeting the first connected display');
    await page.locator('#profile-map-0').fill('DP-1');
    const audioField = page.locator('#profile-map-2');
    if (await audioField.count()) await audioField.fill('venue-sink');
    await page.locator('[data-profile-activate]').click();
    await page.waitForFunction(() => document.querySelector('#profile-message').textContent.startsWith('Activated'));
    await page.waitForFunction(() => config?.committed === true);
    const activation = requests.find(entry => entry.path.endsWith('/activate'));
    assert(activation, 'activation sends its request');
    assert.equal(activation.body.outputMappings['edid:HDMI-A-1:Acme:Panel:42'], 'DP-1');
    assert.equal(activation.body.outputMappings['NOT-CONNECTED'], undefined);
    assert.equal(Object.keys(activation.body.outputMappings).length, 1);
    assert.equal(activation.body.audioMappings['profile-sink'], 'venue-sink');
    assert.equal(activation.headers['if-match'], String(fixture.revision ?? 0));
    assert.equal(activation.headers['if-config-generation'], '7');
    assert.equal(activation.headers['if-config-epoch'], 'profile-fixture');
    assert.equal(activation.headers['x-config-generation'], undefined);
    assert.equal(await page.locator('#tab-profiles dialog[open]').count(), 0,
      'successful activation closes its mapping dialog');

    // A stale activation keeps the dialog open so the operator can inspect
    // the conflict, dismiss it, and resolve it through Save or Revert.
    activationStatus = 409;
    await page.locator('#profile-table button:has-text("Activate")').first().click();
    await page.locator('[data-profile-activate]').click();
    await page.waitForFunction(() => document.querySelector('#toast').textContent.includes('stale configuration version'));
    assert.equal(await page.locator('dialog[open]').count(), 1, 'failed activation leaves the dialog available');
    assert.deepEqual(failures, [], `browser errors: ${failures.join('; ')}`);
    await context.close();
  } finally {
    await browser.close();
  }
  process.stdout.write('System Profiles browser checks passed.\n');
})().catch(error => { console.error(error); process.exitCode = 1; });
