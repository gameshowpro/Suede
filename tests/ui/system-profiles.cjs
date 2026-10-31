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
      apps: [{ id: 'stage', launcher: { kind: 'chromium-kiosk', uri: 'http://control.test/stage' }, audio: { output: 'profile-sink' } },
        { id: 'lobby', launcher: { kind: 'chromium-kiosk', uri: 'http://control.test/lobby' } },
        { id: 'unused', launcher: { kind: 'chromium-kiosk', uri: 'http://control.test/unused' } }],
      activeApp: 'stage' }, assets: {} };
  const fixtureOutputs = [
    { name: 'HDMI-A-1', make: 'Acme', model: 'Panel', serial: '42' }, { name: 'HDMI-A-2' },
    { name: 'HDMI-A-3' }, { name: 'HDMI-A-4' }];
  const fixtureAv = { audioOutputs: [
    { id: 'alpha-sink', description: 'Alpha', isNullSink: false, isDefault: true },
    { id: 'null-sink', description: 'Null', isNullSink: true, isDefault: false },
    { id: 'beta-sink', description: 'Beta', isNullSink: false, isDefault: false }], audioInputs: [], videoInputs: [] };
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
        // The server stores a subset as the boolean `apps: true`.
        profiles.push({ ...body, includes: { ...body.includes, apps: Array.isArray(body.includes.apps) ? body.includes.apps.length > 0 : body.includes.apps } });
        return reply(profiles.at(-1), 201);
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
      if (path === '/av') return reply(fixtureAv);
      if (path === '/outputs') return reply(fixtureOutputs);
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
    // The name check awaits a fresh profile list, so wait for its verdict.
    await page.waitForFunction(() => /already exists/i.test(document.querySelector('#profile-message').textContent));
    assert.equal(requests.filter(entry => entry.path === '/profiles' && entry.method === 'POST').length, 1,
      'creating a profile refuses an existing name after trimming and case folding');
    await page.locator('#profile-editor-cancel').click();

    // Save editor: the Apps checkbox is a tri-state backed by a picker.
    const profilePosts = () => requests.filter(entry => entry.path === '/profiles' && entry.method === 'POST');
    const appsBox = page.locator('[data-profile-include="apps"]');
    const boxState = () => appsBox.evaluate(box => ({ checked: box.checked, indeterminate: box.indeterminate }));
    const pickApp = id => page.locator(`[data-pick-app="${id}"]`);
    const chooseSubset = async () => {
      await page.locator('#profile-apps-choose').click();
      await page.locator('#profile-apps-picker').waitFor({ state: 'visible' });
      await pickApp('renderer-3').uncheck(); await pickApp('renderer-4').uncheck();
      await page.locator('#profile-apps-picker-ok').click();
      await page.locator('#profile-apps-picker').waitFor({ state: 'hidden' });
      // The dialog hides before its close event hands the choice to the editor.
      await page.waitForFunction(() => document.querySelector('#profile-apps-summary').textContent.startsWith('2 of'));
    };
    const saveAs = async name => {
      await page.locator('#profile-name').fill(name);
      await page.locator('#profile-save').click();
      await page.waitForFunction(() => document.querySelector('#profile-editor').hidden);
      return profilePosts().at(-1).body;
    };
    await page.locator('#profile-new').click();
    assert.equal(await page.locator('#profile-apps-summary').textContent(), 'All 4 apps');
    assert.equal(await page.locator('[data-profile-include="apps"][aria-checked]').count(), 0,
      'the native checkbox carries no aria-checked');
    await chooseSubset();
    assert.deepEqual(await boxState(), { checked: false, indeterminate: true });
    assert.match(await page.locator('#profile-apps-summary').textContent(), /^2 of 4 apps/);
    assert.equal(await page.evaluate(() => document.activeElement.id), 'profile-apps-choose',
      'focus returns to the button that opened the picker');
    assert.deepEqual((await saveAs('Subset set')).includes.apps, ['renderer-1', 'renderer-2']);

    await page.locator('#profile-new').click();
    await chooseSubset();
    await appsBox.click();
    assert.deepEqual(await boxState(), { checked: true, indeterminate: false }, 'a click on a subset selects all');
    assert.equal(await page.locator('#profile-apps-summary').textContent(), 'All 4 apps');
    assert.equal((await saveAs('All apps set')).includes.apps, true);
    await page.locator('#profile-new').click();
    await chooseSubset();
    await appsBox.click(); await appsBox.click();
    assert.deepEqual(await boxState(), { checked: false, indeterminate: false });
    assert.equal(await page.locator('#profile-apps-summary').textContent(), 'Apps are not saved.');
    assert.equal((await saveAs('No apps set')).includes.apps, false);

    // The active app must stay among the chosen apps while Active app is saved.
    await page.locator('#profile-new').click();
    await page.locator('#profile-apps-choose').click();
    await pickApp('renderer-1').uncheck();
    await page.locator('#profile-apps-picker-ok').click();
    await page.locator('#profile-apps-picker').waitFor({ state: 'hidden' });
    await page.waitForFunction(() => document.querySelector('#profile-apps-summary').textContent.startsWith('3 of'));
    assert.match(await page.locator('#profile-editor-note').textContent(), /active app .* not among/);
    await page.locator('#profile-name').fill('Missing active');
    const postsBefore = profilePosts().length;
    await page.locator('#profile-save').click();
    assert.equal(profilePosts().length, postsBefore, 'Save refuses while the active app is left out');
    assert.match(await page.locator('#profile-message').textContent(), /not among the selected apps/);
    await page.locator('[data-profile-include="activeApp"]').uncheck();
    assert.equal(await page.locator('#profile-editor-note').textContent(), '');
    assert.equal((await saveAs('Missing active')).includes.activeApp, false);

    // Escape in the picker changes nothing.
    await page.locator('#profile-new').click();
    await page.locator('#profile-apps-choose').click();
    await pickApp('renderer-3').uncheck();
    await page.keyboard.press('Escape');
    await page.locator('#profile-apps-picker').waitFor({ state: 'hidden' });
    assert.deepEqual(await boxState(), { checked: true, indeterminate: false });
    assert.equal(await page.locator('#profile-apps-summary').textContent(), 'All 4 apps');
    await page.locator('#profile-editor-cancel').click();

    // Update snapshot preselects the stored apps that exist on this system.
    await page.locator('#profile-table button:has-text("Update snapshot")').first().click();
    await page.locator('#profile-editor').waitFor({ state: 'visible' });
    assert.deepEqual(await boxState(), { checked: false, indeterminate: false });
    assert.match(await page.locator('#profile-editor-note').textContent(),
      /None of the 3 apps in this profile are configured on this system\./);
    await page.locator('#profile-apps-choose').click();
    assert.equal(await pickApp('stage').isDisabled(), true, 'a stored app missing here cannot be picked');
    await page.keyboard.press('Escape');
    await page.locator('#profile-editor-cancel').click();

    // With no apps configured the picker is unavailable and the summary says why.
    await page.evaluate(() => { window.savedApps = config.apps; config.apps = []; });
    await page.locator('#profile-new').click();
    assert.equal(await page.locator('#profile-apps-choose').isDisabled(), true);
    assert.match(await page.locator('#profile-apps-summary').textContent(), /No apps are configured/);
    await page.locator('#profile-editor-cancel').click();
    await page.evaluate(() => { config.apps = window.savedApps; });

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
    await page.waitForFunction(() => /already exists/i.test(document.querySelector('#profile-import-error').textContent));
    assert.equal(importPosts().length, 0, 'an existing name is refused case-insensitively after trimming');
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
    await page.waitForFunction(() => /already exists/i.test(document.querySelector('#profile-import-error').textContent));
    assert.equal(importPosts().length, 1, 'duplicate also refuses an existing name');
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
    // The spec reverses the earlier rule that an unknown saved display is left
    // blank: every saved display without an exact match now takes the next
    // detected display no other row holds, and its status says so.
    assert.equal(await page.locator('#profile-map-1').inputValue(), 'HDMI-A-2',
      'an unknown display takes the next unassigned connected display');
    await page.locator('#profile-map-0').fill('DP-1');
    await page.locator('#profile-map-2').fill('venue-sink');
    await page.locator('[data-profile-activate]').click();
    await page.waitForFunction(() => document.querySelector('#profile-message').textContent.startsWith('Activated'));
    await page.waitForFunction(() => config?.committed === true);
    const activation = requests.find(entry => entry.path.endsWith('/activate'));
    assert(activation, 'activation sends its request');
    assert.deepEqual(activation.body.outputMappings, { 'edid:HDMI-A-1:Acme:Panel:42': 'DP-1', 'NOT-CONNECTED': 'HDMI-A-2' });
    assert.equal(activation.body.audioMappings['profile-sink'], 'venue-sink');
    assert.deepEqual(activation.body.sections, { displays: true, backgrounds: true, apps: true, activeApp: true, settings: true },
      'an untouched dialog activates every section');
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
    await page.locator('[data-profile-cancel]').click();
    activationStatus = 200;

    // Sections the profile lacks are disabled; apps can be limited to a subset.
    const activations = () => requests.filter(entry => entry.path.endsWith('/activate'));
    const full = { ...exported.includes };
    const openActivate = async includes => {
      exported.includes = includes;
      await page.locator('#profile-table button:has-text("Activate")').first().click();
      await page.locator('#profile-activate-dialog').waitFor({ state: 'visible' });
    };
    const section = key => page.locator(`[data-profile-section="${key}"]`);
    const activate = async () => {
      const before = activations().length;
      await page.locator('[data-profile-activate]').click();
      await page.waitForFunction(() => !document.querySelector('#profile-activate-dialog').open);
      assert.equal(activations().length, before + 1);
      return activations().at(-1).body;
    };
    await openActivate({ ...full, activeApp: false });
    assert.equal(await section('activeApp').isDisabled(), true);
    assert.equal(await section('activeApp').isChecked(), false);
    assert.match(await section('activeApp').locator('xpath=ancestor::label').textContent(), /not in this profile/);
    assert.equal(await section('apps').isDisabled(), false);
    await page.locator('#profile-activate-apps-choose').click();
    await pickApp('lobby').uncheck(); await pickApp('unused').uncheck();
    await page.locator('#profile-apps-picker-ok').click();
    await page.locator('#profile-apps-picker').waitFor({ state: 'hidden' });
    await page.waitForFunction(() => document.querySelector('#profile-activate-apps-summary').textContent === '1 of 3 apps');
    assert.equal(await page.evaluate(() => document.activeElement.id), 'profile-activate-apps-choose');
    assert.deepEqual(await section('apps').evaluate(box => ({ checked: box.checked, indeterminate: box.indeterminate })),
      { checked: false, indeterminate: true });
    assert.equal(await page.locator('#profile-activate-apps-summary').textContent(), '1 of 3 apps');
    assert.match(await page.locator('#profile-activate-apps-hint').textContent(), /Adds or updates the 1 selected app/);
    assert.equal(await page.locator('#profile-map-audio-rows input').count(), 1, 'one audio row for the one chosen app');
    let body = await activate();
    assert.deepEqual(body.sections, { displays: true, backgrounds: true, apps: ['stage'], activeApp: false, settings: true });

    await openActivate(full);
    assert.equal(await section('activeApp').isDisabled(), false);
    await section('apps').click();
    assert.equal(await section('apps').isChecked(), false);
    assert.equal(await page.locator('#profile-activate-apps-hint').textContent(), "Leaves this system's apps unchanged.");
    assert.equal(await page.locator('#profile-map-audio-rows input').count(), 0, 'no audio rows when no apps are applied');
    assert.match(await page.locator('#profile-activate-error').textContent(), /active app "stage" would not be in this system's app list/);
    const refused = activations().length;
    await page.locator('[data-profile-activate]').click();
    assert.equal(activations().length, refused, 'activation refuses an active app that would not exist');
    await section('activeApp').uncheck();
    assert.equal(await page.locator('#profile-activate-error').textContent(), '');
    await section('displays').uncheck();
    assert.equal(await page.locator('#profile-map-displays-rows').isHidden(), true, 'display rows hide with their section');
    body = await activate();
    assert.deepEqual(body.sections, { displays: false, backgrounds: true, apps: false, activeApp: false, settings: true });
    assert.deepEqual(body.outputMappings, {});

    await openActivate(full);
    assert.equal(await page.locator('#profile-activate-apps-hint').textContent(), 'Replaces this system\'s app list with the 3 apps in this profile.');
    await page.locator('[data-profile-cancel]').click();
    exported.includes = full;

    // Device mapping rows are native combos over the detected devices.
    const mapField = n => page.locator(`#profile-map-${n}`);
    const mapStatus = n => page.locator(`#profile-map-${n}-status`);
    const mapError = () => page.locator('#profile-activate-error').textContent();
    const optionsOf = id => page.locator(`#${id} option`).evaluateAll(options => options.map(o => [o.value, o.textContent]));
    const mapValues = () => Promise.all([0, 1, 2].map(n => mapField(n).inputValue()));
    await openActivate(full);
    assert.match(await page.locator('#profile-map-fieldset > p.hint').textContent(),
      /^The profile refers to displays and audio outputs by the names they had on the system where it was saved\..*Leave a field blank to keep the saved name\.$/);
    assert.equal(await mapField(0).getAttribute('list'), 'profile-map-displays');
    assert.equal(await mapField(1).getAttribute('list'), 'profile-map-displays');
    assert.equal(await mapField(2).getAttribute('list'), 'profile-map-audio');
    assert.deepEqual(await optionsOf('profile-map-displays'),
      [['HDMI-A-1', 'Acme Panel 42'], ['HDMI-A-2', ''], ['HDMI-A-3', ''], ['HDMI-A-4', '']],
      'the display list holds every detected output in daemon order');
    assert.deepEqual(await optionsOf('profile-map-audio'), [['alpha-sink', 'Alpha (default)'], ['beta-sink', 'Beta']],
      'the audio list leaves out the null sink');
    assert.equal(await page.locator('label[for="profile-map-0"]').textContent(), 'Saved display HDMI-A-1 · Acme Panel 42');
    assert.equal(await page.locator('label[for="profile-map-1"]').textContent(), 'Saved display NOT-CONNECTED');
    assert.equal(await page.locator('label[for="profile-map-2"]').textContent(), 'Saved audio output profile-sink');
    assert.equal(await mapField(0).getAttribute('placeholder'), 'Keep HDMI-A-1');
    assert.equal(await mapField(2).getAttribute('placeholder'), 'Keep profile-sink');
    assert.equal(await mapField(0).getAttribute('aria-describedby'), 'profile-map-0-status');
    assert.equal(await mapStatus(0).getAttribute('data-profile-map-status'), '');
    // Default assignment: the EDID rule matches HDMI-A-1 exactly, the unknown
    // display takes the next unassigned output, and the unknown sink the
    // first sink that is not the null sink.
    assert.deepEqual(await mapValues(), ['HDMI-A-1', 'HDMI-A-2', 'alpha-sink']);
    assert.equal(await mapStatus(0).textContent(), 'matches a connected display');
    assert.match(await mapStatus(1).textContent(), /^suggested: the next unassigned display$/);
    assert.equal(await mapStatus(2).textContent(), 'suggested: the next unassigned audio output');
    body = await activate();
    assert.deepEqual(body.outputMappings, { 'NOT-CONNECTED': 'HDMI-A-2' },
      'a display the saved EDID rule already selects sends no mapping, so the rule and its adopted values stay');
    assert.deepEqual(body.audioMappings, { 'profile-sink': 'alpha-sink' });

    // Pointing the EDID rule at a different display sends it; a blank field keeps the saved name.
    await openActivate(full);
    await mapField(0).fill('HDMI-A-3');
    assert.equal(await mapStatus(0).textContent(), '', 'an edited field no longer shows the suggestion status');
    await mapField(1).fill('');
    await mapField(2).fill('');
    body = await activate();
    assert.deepEqual(body.outputMappings, { 'edid:HDMI-A-1:Acme:Panel:42': 'HDMI-A-3' });
    assert.deepEqual(body.audioMappings, {});

    // One connector cannot take the place of two saved displays.
    await openActivate(full);
    await mapField(0).fill('HDMI-A-3');
    await mapField(1).fill('HDMI-A-3');
    assert.match(await mapError(), /^HDMI-A-3 is chosen for two saved displays\. Each display on this system can take the place of one saved display\.$/);
    const beforeDuplicate = activations().length;
    await page.locator('[data-profile-activate]').click();
    await page.waitForFunction(() => document.querySelector('#toast').textContent.includes('chosen for two saved displays'));
    assert.equal(activations().length, beforeDuplicate, 'activation refuses a connector chosen twice');
    // Typed values survive hiding the rows and an Apps round trip.
    await mapField(2).fill('beta-sink');
    await section('displays').uncheck();
    assert.equal(await mapError(), '', 'hidden display rows are not checked for duplicates');
    await section('displays').check();
    await section('apps').click(); await section('apps').click();
    assert.deepEqual(await mapValues(), ['HDMI-A-3', 'HDMI-A-3', 'beta-sink']);
    assert.match(await mapError(), /chosen for two saved displays/);
    await page.locator('#profile-map-reset').click();
    assert.deepEqual(await mapValues(), ['HDMI-A-1', 'HDMI-A-2', 'alpha-sink'], 'Reset to suggested restores the defaults');
    assert.equal(await mapStatus(0).textContent(), 'matches a connected display');
    assert.equal(await mapError(), '');
    await page.locator('[data-profile-cancel]').click();

    // With every detected display taken, the unknown display keeps its saved name.
    await page.evaluate(() => { window.savedOutputs = outputs; outputs = outputs.slice(0, 1); });
    await openActivate(full);
    assert.deepEqual(await optionsOf('profile-map-displays'), [['HDMI-A-1', 'Acme Panel 42']]);
    assert.equal(await mapField(1).inputValue(), '');
    assert.equal(await mapStatus(1).textContent(), 'no unassigned display is connected; the saved name is kept');
    body = await activate();
    assert.deepEqual(body.outputMappings, {});
    await page.evaluate(() => { outputs = window.savedOutputs; });

    // The assignment itself is a pure function of rows, outputs and sinks.
    const assigned = await page.evaluate(() => profileDefaultMappings([
      { kind: 'display', match: { make: 'Acme', model: 'Panel' } },
      { kind: 'display', match: { make: 'Acme', model: 'Panel' } },
      { kind: 'display', match: { name: 'hdmi-a-1' } },
      { kind: 'audio', name: 'beta-sink' }, { kind: 'audio', name: 'gone' }, { kind: 'audio', name: 'gone-too' },
    ], [{ name: 'HDMI-A-1', make: 'Acme', model: 'Panel' }, { name: 'HDMI-A-2' }, { name: 'HDMI-A-3', make: 'Acme', model: 'Panel' }],
    [{ id: 'null-sink', isNullSink: true }, { id: 'alpha-sink', isNullSink: false }, { id: 'beta-sink', isNullSink: false }]));
    assert.deepEqual(assigned.map(entry => entry.value), ['HDMI-A-1', 'HDMI-A-3', 'HDMI-A-2', 'beta-sink', 'alpha-sink', ''],
      'identical panels take the first unused match, names compare case-sensitively, and a run out of devices stays blank');
    assert.deepEqual(assigned.map(entry => entry.status), ['matches a connected display', 'matches a connected display',
      'suggested: the next unassigned display', 'matches a detected audio output',
      'suggested: the next unassigned audio output', 'no unassigned audio output is detected; the saved name is kept']);
    const held = await page.evaluate(() => profileDefaultMappings([{ kind: 'audio', name: 'x', held: 'alpha-sink' }, { kind: 'audio', name: 'y' }],
      [], [{ id: 'alpha-sink', isNullSink: false }, { id: 'beta-sink', isNullSink: false }]));
    assert.deepEqual(held, [{ value: 'alpha-sink', status: null }, { value: 'beta-sink', status: 'suggested: the next unassigned audio output' }],
      'a value already in a field counts as assigned');

    assert.deepEqual(failures, [], `browser errors: ${failures.join('; ')}`);
    await context.close();
  } finally {
    await browser.close();
  }
  process.stdout.write('System Profiles browser checks passed.\n');
})().catch(error => { console.error(error); process.exitCode = 1; });
