// Captures the web UI's Displays tab from a live appliance for the docs.
// Run through scripts/capture-screenshots.sh, which encodes the results and
// updates the image sizes in the docs; see README.md in this directory.
//
// Usage: node capture.cjs <base-url> <out-dir> <healthy|disconnected|profiles>
//
// The viewport is the narrowest width at which the page shows two columns,
// read from the page's own stylesheet, at 100% scale, so the captures follow
// the layout if the breakpoint moves. The healthy phase switches the page's
// mode toggle to take each mode's captures; that edits the appliance's working
// copy, which is put back exactly as it was afterwards (an unsaved working copy
// is rewritten, a clean one is reverted) and checked. The profiles phase saves
// a temporary profile on the appliance and deletes it afterwards.
//
// SCREENSHOT_REDACTIONS is a JSON array of [pattern, replacement] pairs
// (case-insensitive regular expressions, matched only where not touching other
// letters). The page's text, tooltips and field values are rewritten with them
// as the page renders, and a capture that still shows a match is an error.
// The wrapper builds the list from local files that are not in the repository.
const { chromium } = require('playwright');
const fs = require('node:fs');
const path = require('node:path');

const [base, out, phase] = process.argv.slice(2);
if (!base || !out || !['healthy', 'disconnected', 'profiles'].includes(phase)) {
  console.error('usage: node capture.cjs <base-url> <out-dir> <healthy|disconnected|profiles>');
  process.exit(2);
}
const api = (p) => new URL(`/api/v1${p}`, base).href;
const redactions = JSON.parse(process.env.SCREENSHOT_REDACTIONS || '[]');
// SCREENSHOT_ONLY: comma-separated shot names to take instead of the whole phase.
const only = process.env.SCREENSHOT_ONLY ? process.env.SCREENSHOT_ONLY.split(',').map(s => s.trim()) : null;

// The temporary profile the profiles phase saves and shows.
const TEMP_PROFILE = { name: 'Evening show', description: 'Quad split with three apps' };

// What each phase captures. `full` is the whole page; `el` is the union of the
// listed elements' boxes plus `pad`; `dialog` is a modal dialog, captured on
// its own. `tab` is the tab to show, `open` runs before the capture and `close`
// after it. The mode a capture needs is set before it is taken. `pins` types destination
// pins into the warp editor's fields for that capture only (corners in
// top-left, top-right, bottom-right, bottom-left order, then the center
// fractions), so the editor shows a warped picture; the pins are reset after.
const ARRANGE = { dialog: '#arrange-dialog',
  open: async (page) => { await page.click('#arrange-outputs'); await page.waitForTimeout(1500); },
  close: async (page) => { await page.click('#arrange-cancel'); await page.waitForTimeout(500); } };

const SHOTS = {
  healthy: [
    { name: 'displays', mode: 'warp', full: true },
    { name: 'details/edge-blending', mode: 'warp', el: ['#edge-blending-panel'], pad: 0 },
    { name: 'details/test-pattern', mode: 'warp', el: ['#pattern-row'], pad: 10 },
    { name: 'details/warp-editor', mode: 'warp', el: ['#output-warp'], pad: 10,
      pins: { corners: [[0.06, 0.05], [0.96, 0.01], [0.99, 0.97], [0.02, 0.92]], center: [0.46, 0.54] } },
    { name: 'details/slice-from-canvas', mode: 'warp', el: ['#shared-content'], pad: 10 },
    { name: 'details/arrange-simple', mode: 'warp', ...ARRANGE },
    { name: 'details/arrange-exact', mode: 'exact', ...ARRANGE },
  ],
  disconnected: [
    { name: 'displays-disconnected', full: true },
    { name: 'details/unplugged-outputs', el: ['#layout', '#output-table'], pad: 12 },
    { name: 'details/warning-banners', el: ['#alerts'], pad: 0 },
  ],
  // Needs a clean working copy: the page refuses to activate over unsaved edits.
  profiles: [
    { name: 'profiles', tab: 'profiles', el: ['#tab-profiles > .panel:first-child'], pad: 10 },
    { name: 'details/profile-apps-picker', tab: 'profiles', dialog: '#profile-apps-picker',
      open: async (page) => {
        await page.click('#profile-new');
        await page.fill('#profile-name', 'Late show');
        await page.click('#profile-apps-choose');
        await page.locator('#profile-apps-picker').waitFor({ state: 'visible' });
        // Leave out the second to fourth apps, keeping the active one, and
        // show the top of the list.
        const active = (await getJson('/config')).activeApp;
        const boxes = page.locator(`#profile-apps-picker-list input[type=checkbox]:not(:disabled):not([data-pick-app="${active}"])`);
        for (const i of [1, 2, 3]) await boxes.nth(i).uncheck();
        await page.evaluate(() => { document.querySelector('#profile-apps-picker-list').scrollTop = 0; });
      },
      close: async (page) => {
        await page.click('#profile-apps-picker-ok');
        await page.locator('#profile-apps-picker').waitFor({ state: 'hidden' });
        await page.waitForTimeout(300);
      } },
    { name: 'details/profile-save', tab: 'profiles', el: ['#profile-editor'], pad: 10,
      close: async (page) => { await page.click('#profile-editor-cancel'); } },
    { name: 'details/profile-activate', tab: 'profiles', dialog: '#profile-activate-dialog',
      open: async (page) => {
        await page.locator('#profile-table tr', { hasText: TEMP_PROFILE.name }).getByRole('button', { name: 'Activate' }).click();
        await page.locator('#profile-activate-dialog').waitFor({ state: 'visible' });
        await page.click('#profile-activate-apps-choose');
        await page.locator('#profile-apps-picker').waitFor({ state: 'visible' });
        const active = (await getJson('/config')).activeApp;
        await page.locator(`#profile-apps-picker-list input[type=checkbox]:not([data-pick-app="${active}"])`).first().uncheck();
        await page.click('#profile-apps-picker-ok');
        await page.locator('#profile-apps-picker').waitFor({ state: 'hidden' });
        await page.waitForTimeout(500);
        await page.mouse.move(0, 0);
      },
      close: async (page) => { await page.click('#profile-activate-dialog [data-profile-cancel]'); } },
  ],
};

async function getJson(p) {
  const res = await fetch(api(p));
  if (!res.ok) throw new Error(`GET ${p}: ${res.status}`);
  return res.json();
}

async function restore(before) {
  const res = before.committed === false
    ? await fetch(api('/config'), { method: 'PUT', headers: { 'content-type': 'application/json' },
      body: JSON.stringify({ ...before, committed: false }) })
    : await fetch(api('/config/revert'), { method: 'POST' });
  if (!res.ok) throw new Error(`restoring the working copy: ${res.status} ${await res.text()}`);
  const after = await getJson('/config');
  if (JSON.stringify(after) !== JSON.stringify(before)) {
    throw new Error('the working copy differs from before the capture; compare GET /api/v1/config with the backup');
  }
  console.log(`working copy restored (${before.committed === false ? 'unsaved edits rewritten' : 'reverted to saved'})`);
}

async function twoColumnWidth(page) {
  return page.evaluate(() => {
    for (const sheet of document.styleSheets) {
      for (const rule of sheet.cssRules) {
        if (rule instanceof CSSMediaRule && /--panel-columns:\s*2/.test(rule.cssText)) {
          const m = rule.conditionText.match(/min-width:\s*(\d+)px/);
          if (m) return Number(m[1]);
        }
      }
    }
    return null;
  });
}

// Runs in the page: rewrites every text node, tooltip, label and field value
// now and on every later change, so live updates are rewritten too.
function installRedaction(pairs) {
  const rules = pairs.map(([p, r]) => [new RegExp(`(?<![a-z])(?:${p})(?![a-z])`, 'gi'), r]);
  const fix = (v) => rules.reduce((t, [re, r]) => t.replace(re, r), v);
  const visit = (root) => {
    const walker = document.createTreeWalker(root, NodeFilter.SHOW_TEXT);
    for (let n = walker.nextNode(); n; n = walker.nextNode()) {
      if (n.parentElement?.closest('script, style')) continue;
      const v = fix(n.data);
      if (v !== n.data) n.data = v;
    }
    const scope = root.querySelectorAll ? root : document;
    for (const el of scope.querySelectorAll('[title], [aria-label], input, textarea')) {
      for (const a of ['title', 'aria-label']) {
        const v = el.getAttribute(a);
        if (v != null && fix(v) !== v) el.setAttribute(a, fix(v));
      }
      if ('value' in el && el.type !== 'checkbox' && el.type !== 'radio' && fix(el.value) !== el.value) el.value = fix(el.value);
    }
  };
  window.__redactVisit = () => visit(document.body);
  const start = () => {
    visit(document.body);
    new MutationObserver(() => visit(document.body))
      .observe(document.body, { subtree: true, childList: true, characterData: true, attributes: true, attributeFilter: ['title', 'aria-label'] });
  };
  document.readyState === 'loading' ? document.addEventListener('DOMContentLoaded', start) : start();
}

async function checkRedacted(page, label) {
  if (!redactions.length) return;
  await page.evaluate(() => window.__redactVisit?.());
  const text = await page.evaluate(() => [document.body.innerText,
    ...[...document.querySelectorAll('svg text')].map(e => e.textContent),
    ...[...document.querySelectorAll('[title], [aria-label]')].map(e => `${e.title ?? ''} ${e.getAttribute('aria-label') ?? ''}`),
    ...[...document.querySelectorAll('input, select, textarea')].map(e => e.value)].join(' '));
  for (const [p] of redactions) {
    const hit = text.match(new RegExp(`(?<![a-z])(?:${p})(?![a-z])`, 'i'));
    if (hit) throw new Error(`${label}: "${hit[0]}" is still visible after redaction`);
  }
}

async function setMode(page, mode) {
  if (!mode || (await page.getAttribute(`#mode-${mode}`, 'aria-pressed')) === 'true') return;
  await page.click(`#mode-${mode}`);
  await page.waitForFunction((m) => document.querySelector(`#mode-${m}`)?.getAttribute('aria-pressed') === 'true', mode);
  await page.waitForTimeout(4000);
}

async function setField(page, id, value) {
  // The editor rebuilds its fields after every accepted edit, so look each one
  // up afresh.
  await page.evaluate(([id, value]) => {
    const input = document.getElementById(id);
    input.value = String(value);
    input.dispatchEvent(new Event('change', { bubbles: true }));
  }, [id, value]);
  await page.waitForTimeout(200);
}

async function setPins(page, pins) {
  for (const [i, corner] of pins.corners.entries()) {
    for (const [a, v] of corner.entries()) await setField(page, `pin-${i}-${a}`, v);
  }
  for (const [a, v] of pins.center.entries()) await setField(page, `center-${a}`, v);
  const shown = await page.evaluate(() => document.querySelector('#warp-error')?.textContent?.trim());
  if (shown) throw new Error(`the warp editor rejected the pins: ${shown}`);
  await page.waitForTimeout(3000);
}

async function showTab(page, tab) {
  await page.click(`[role=tab][data-tab="${tab}"]`);
  await page.locator(`#tab-${tab}`).waitFor({ state: 'visible' });
  await page.waitForTimeout(500);
}

// Saves the temporary profile from the live configuration, with a subset of
// its apps that keeps the active one, and returns its id.
async function saveTempProfile(config) {
  const ids = (config.apps ?? []).map(a => a.id);
  const apps = [...new Set([config.activeApp, ...ids].filter(Boolean))].slice(0, 3);
  const id = crypto.randomUUID();
  const res = await fetch(api('/profiles'), { method: 'POST', headers: { 'content-type': 'application/json' },
    body: JSON.stringify({ id, ...TEMP_PROFILE, includes: { displays: true, backgrounds: true, apps, activeApp: true, settings: true } }) });
  if (!res.ok) throw new Error(`saving the temporary profile: ${res.status} ${await res.text()}`);
  return id;
}

async function deleteTempProfile(id) {
  const res = await fetch(api(`/profiles/${encodeURIComponent(id)}`), { method: 'DELETE' });
  if (!res.ok) throw new Error(`deleting the temporary profile ${id}: ${res.status} ${await res.text()}`);
  console.log('temporary profile deleted');
}

async function capture(page, shot) {
  const file = path.join(out, `${shot.name}.png`);
  fs.mkdirSync(path.dirname(file), { recursive: true });
  if (shot.tab) await showTab(page, shot.tab);
  await setMode(page, shot.mode);
  if (shot.pins) await setPins(page, shot.pins);
  if (shot.open) await shot.open(page);
  await checkRedacted(page, shot.name);
  if (shot.full) {
    await page.screenshot({ path: file, fullPage: true });
  } else if (shot.dialog) {
    await page.locator(shot.dialog).screenshot({ path: file });
  } else {
    const boxes = [];
    for (const s of shot.el) {
      await page.locator(s).first().scrollIntoViewIfNeeded();
      await page.waitForTimeout(300);
      const b = await page.locator(s).first().boundingBox();
      if (!b) throw new Error(`${shot.name}: ${s} is not visible`);
      const scroll = await page.evaluate(() => ({ x: scrollX, y: scrollY }));
      boxes.push({ x: b.x + scroll.x, y: b.y + scroll.y, width: b.width, height: b.height });
    }
    const pad = shot.pad ?? 0;
    const x0 = Math.max(0, Math.min(...boxes.map(b => b.x)) - pad), y0 = Math.max(0, Math.min(...boxes.map(b => b.y)) - pad);
    const x1 = Math.max(...boxes.map(b => b.x + b.width)) + pad, y1 = Math.max(...boxes.map(b => b.y + b.height)) + pad;
    await page.screenshot({ path: file, fullPage: true,
      clip: { x: Math.round(x0), y: Math.round(y0), width: Math.round(x1 - x0), height: Math.round(y1 - y0) } });
  }
  if (shot.close) await shot.close(page);
  if (shot.pins) {
    await page.click('#warp-reset-pins');
    await page.click('#warp-reset-centers');
    await page.waitForTimeout(1500);
  }
  console.log(`captured ${shot.name}.png`);
}

(async () => {
  const before = await getJson('/config');
  fs.writeFileSync(path.join(out, 'working-copy-backup.json'), JSON.stringify(before, null, 2));
  if (phase === 'profiles' && before.committed === false) {
    console.error('the profiles phase needs a clean working copy; save or revert the appliance first');
    process.exit(1);
  }
  const browser = await chromium.launch({ headless: true });
  let failed = null, tempProfile = null;
  try {
    const context = await browser.newContext({ viewport: { width: 1280, height: 1200 }, colorScheme: 'dark', deviceScaleFactor: 1 });
    const page = await context.newPage();
    if (phase === 'profiles') tempProfile = await saveTempProfile(before);
    const errors = [];
    page.on('pageerror', e => errors.push(e.message));
    page.on('dialog', d => { console.log(`page dialog accepted: ${d.message()}`); d.accept(); });
    await page.addInitScript(() => localStorage.setItem('suede.feeNoticeAcknowledged', '1'));
    // The sticky header would cover the top of a crop scrolled to the top of
    // the window; unstuck, it still heads the full-page captures.
    await page.addInitScript(() => document.addEventListener('DOMContentLoaded', () => {
      const style = document.createElement('style');
      style.textContent = 'header { position: static !important; }';
      document.head.append(style);
    }));
    if (redactions.length) await page.addInitScript(installRedaction, redactions);
    await page.goto(base, { waitUntil: 'load' });
    const width = await twoColumnWidth(page);
    if (!width) throw new Error('could not find the two-column breakpoint in the page stylesheet');
    await page.setViewportSize({ width, height: 1200 });
    console.log(`viewport ${width} px (two-column breakpoint)`);
    // Let telemetry arrive so the chips and charts are populated.
    await page.waitForTimeout(5000);
    await page.locator('#layout .display-box').first().click();
    await page.waitForTimeout(2000);
    await page.mouse.move(0, 0);
    const shots = SHOTS[phase].filter(s => !only || only.includes(s.name));
    if (!shots.length) throw new Error(`no ${phase} shot is named ${only.join(', ')}`);
    for (const shot of shots) await capture(page, shot);
    if (errors.length) throw new Error(`page errors: ${errors.join('; ')}`);
  } catch (e) {
    failed = e;
  } finally {
    await browser.close();
    if (tempProfile) try { await deleteTempProfile(tempProfile); } catch (e) { failed ??= e; console.error(e.message); }
    try { await restore(before); } catch (e) { failed ??= e; console.error(e.message); }
  }
  if (failed) { console.error(failed.message ?? failed); process.exit(1); }
})();
