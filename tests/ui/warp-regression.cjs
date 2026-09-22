// Real Chromium interaction tests against a deterministic public-API fixture.
// Run: NODE_PATH=/path/to/playwright/node_modules node tests/ui/warp-editor.cjs
const { chromium } = require('playwright');
const fs = require('node:fs');
const assert = require('node:assert/strict');
const { createHash } = require('node:crypto');
const fixture = JSON.parse(fs.readFileSync('docs/examples/four-output-warp.json'));
const html = fs.readFileSync('src/api/ui/index.html','utf8');
const sleep = ms => new Promise(r=>setTimeout(r,ms));
(async()=>{
  const browser = await chromium.launch({headless:true});
  let current=structuredClone(fixture), committed=structuredClone(fixture), generation=0;
  let epoch="fixture-epoch";
  let delay=0, inFlight=0, maxFlight=0, history=[], rejection=false, recommendationDelay=0;
  const failures=[], results=[];
  const context = await browser.newContext({viewport:{width:1280,height:1000}});
  await context.route('**/*', async route=>{
    const req=route.request(), url=new URL(req.url()), path=url.pathname.replace('/api/v1','');
    const respond=(body,status=200,headers={})=>route.fulfill({status,contentType:'application/json',headers,body:JSON.stringify(body)});
    if (url.pathname==='/') return route.fulfill({contentType:'text/html',body:html});
    if(path==='/config' && req.method()==='GET') return respond(current,200,{'x-config-generation':String(generation),'x-config-epoch':epoch,etag:`"${current.revision}"`});
    if(path.startsWith('/config') && req.method()!=='GET') {
      inFlight++; maxFlight=Math.max(maxFlight,inFlight);
      const headers=req.headers(), data=req.postDataJSON();
      history.push({path,method:req.method(),data,time:Date.now()});
      await sleep(delay);
      let status=200, body;
      // Round 4: there is one shared working copy and writes are unconditional.
      // The UI itself no longer sends any of these three headers, but a
      // precondition is still honored when a caller (a script, `curl`) opts
      // into naming a version — and rejected only then, on a stale value.
      const hasPrecondition=headers['if-config-epoch']!=null || headers['if-config-generation']!=null || headers['if-match']!=null;
      if(hasPrecondition && (headers['if-config-epoch']!==epoch || headers['if-config-generation']!==String(generation) || Number(headers['if-match'])!==current.revision)) {status=409;body={detail:'Working copy changed elsewhere'};}
      else if(rejection) {status=422;body={detail:'Fixture rejects geometry'};rejection=false;}
      else {
        if(path==='/config/revert') current=structuredClone(committed);
        else {
          current=structuredClone(data);
          if(data.committed) {current.revision++; committed=structuredClone(current);}
        }
        generation++; body=current;
      }
      inFlight--;
      return respond(body,status,{'x-config-generation':String(generation),'x-config-epoch':epoch,etag:`"${current.revision}"`});
    }
    if(path==='/projection/stats') return respond({running:true,geometry:{warpAvailable:true,requestedMode:current.projection.mode,effectiveMode:current.projection.mode,retainedWarp:true},control:{session:'test-session'}});
    if(path==='/projection/recommendation') {
      const result={revision:current.revision,generation,requestedAspect:current.projection.canvas.aspect,idealWidth:8000,idealHeight:1125,admissibleWidth:8000,admissibleHeight:1125,presets:[.25,.5,.75,1].map(scale=>({width:8000*scale,height:Math.round(8000*scale/current.projection.canvas.aspect),scale,available:true})),knownLimits:{maxDimension:32768,maxCanvasPixels:64000000},unknownLimits:['Device allocation'],warnings:['Approximate sampled estimate']};
      await sleep(recommendationDelay); return respond(result);
    }
    if(path==='/av') return respond({audioOutputs:[],audioInputs:[],videoInputs:[]});
    if(path==='/system') return respond({suedeVersion:'test',uptimeSeconds:1,packages:[]});
    if(path==='/status') return respond({divergences:[]});
    return respond([]);
  });
  const page=await context.newPage();
  page.on('pageerror',e=>failures.push(e.message));
  await page.goto('http://suede.test/?nosse');
  await page.waitForFunction(()=>configGeneration==='0' && document.querySelectorAll('[data-handle]').length===8);
  await page.locator('#output-table tbody tr').first().click();
  const settle=()=>page.evaluate(()=>flushPreviews());
  const read=()=>page.evaluate(()=>structuredClone(config));
  const mark=name=>{results.push(name);console.log('PASS',name);};
  const point=async(index)=>page.locator(`[data-handle="${index}"]`).evaluate(el=>{
    const p=new DOMPoint(+el.getAttribute('cx'),+el.getAttribute('cy')).matrixTransform(el.ownerSVGElement.getScreenCTM());return {x:p.x,y:p.y};
  });
  await page.locator('#warp-editor').scrollIntoViewIfNeeded();
  const original=await read(), p=await point(0);
  await page.mouse.move(p.x,p.y); await page.mouse.down();
  await page.mouse.move(p.x+25,p.y+20,{steps:8}); await page.mouse.up(); await settle();
  let edited=await read(); assert(edited.outputs[0].geometry.corners[0][0]>0);
  assert.deepEqual(edited.projection.canvas,original.projection.canvas);
  assert.deepEqual(edited.outputs[0].geometry.source,original.outputs[0].geometry.source);
  assert.deepEqual(edited.outputs.slice(1),original.outputs.slice(1));
  mark('real corner pointer drag preserves source, canvas, and neighbors');
  const priorRequests=history.length;
  await page.evaluate(()=>moveHandle(0,structuredClone(warpOutput().geometry.corners[0])));await settle();
  assert.equal(history.length,priorRequests);mark('repeated final coordinates do not dispatch a redundant preview');
  await page.evaluate(()=>editGeometry(g=>g.corners=[[.12,.1],[.85,.03],[.96,.9],[.03,.96]])); await settle();
  const expected=await page.evaluate(()=>{
    const target=projectPoint(homography(unitPins,warpOutput().geometry.corners),[.63,0]);
    const p=new DOMPoint(...target).matrixTransform(warpSvg.getScreenCTM());return {x:p.x,y:p.y};
  });
  const cp=await point(4);await page.mouse.move(cp.x,cp.y);await page.mouse.down();await page.mouse.move(expected.x,expected.y);await page.mouse.up();await settle();
  edited=await read();assert(Math.abs(edited.outputs[0].geometry.center[0]-.63)<.003);
  const pair=await page.evaluate(()=>projectPoint(homography(warpOutput().geometry.corners,unitPins),handlePoints(warpOutput().geometry)[5]));
  assert(Math.abs(pair[0]-.63)<.003);mark('perspective center drag uses inverse homography and moves paired endpoint');
  const valid=structuredClone(edited.outputs[0].geometry);
  await page.evaluate(()=>moveHandle(0,[1.2,1.2]));await settle();
  assert.deepEqual((await read()).outputs[0].geometry,valid);
  assert(await page.locator('#warp-error').textContent());mark('invalid crossing keeps last valid shape and shows rejection');
  await page.locator('#pin-0-0').fill('0.15');await page.locator('#pin-0-0').press('Tab');await settle();
  assert.equal((await read()).outputs[0].geometry.corners[0][0],.15);
  await page.locator('[data-handle="0"]').focus();await page.keyboard.press('ArrowRight');await page.keyboard.press('Shift+ArrowRight');await settle();
  assert(Math.abs((await read()).outputs[0].geometry.corners[0][0]-.161)<1e-9);mark('numeric and fine/coarse keyboard controls');
  await page.locator('#warp-reset-centers').click();await settle();assert.deepEqual((await read()).outputs[0].geometry.center,[.5,.5]);
  await page.locator('#warp-reset-pins').click();await page.waitForFunction(()=>warpOutput().geometry.corners[0][0]===0);await settle();assert.deepEqual((await read()).outputs[0].geometry.corners,fixture.outputs[0].geometry.corners);
  assert.equal(current.revision,0);mark('canonical pin reset and center reset remain unsaved');
  await page.locator('#warp-editor').scrollIntoViewIfNeeded();
  for(let i=0;i<8;i++) {
    const before=JSON.stringify((await read()).outputs[0].geometry), p=await point(i);
    const delta=i<4 ? [[5,4],[-5,4],[-5,-4],[5,-4]][i] : i<6 ? [5,0] : [0,4];
    await page.mouse.move(p.x,p.y);await page.mouse.down();await page.mouse.move(p.x+delta[0],p.y+delta[1]);await page.mouse.up();await settle();
    assert.notEqual(JSON.stringify((await read()).outputs[0].geometry),before,`handle ${i} did not move`);
  }
  await page.locator('#warp-reset-centers').click();await settle();
  await page.locator('#warp-reset-pins').click();await page.waitForFunction(()=>warpOutput().geometry.corners[0][0]===0);await settle();
  mark('all four corners and all four center endpoints respond to real pointer input');
  delay=120;history=[];maxFlight=0;
  await page.evaluate(async()=>{for(let i=0;i<40;i++){editGeometry(g=>g.corners[0][0]=.03+i*.001);await new Promise(r=>setTimeout(r,8));}});
  await settle(); assert.equal(maxFlight,1);assert.equal(current.outputs[0].geometry.corners[0][0],.069);
  assert(history.length<10);for(let i=1;i<history.length;i++)assert(history[i].time-history[i-1].time>=45);
  mark('bounded preview coalescing, sustained progress, final state, and 20Hz ceiling');
  await page.evaluate(()=>editGeometry(g=>g.corners[0][0]=.08));await sleep(30);
  await page.locator('#pj-save').click();await page.waitForFunction(()=>!configBusy);await sleep(180);
  assert.equal(history.at(-1).data.committed,true);assert.equal(current.outputs[0].geometry.corners[0][0],.08);
  await page.evaluate(()=>editGeometry(g=>g.corners[0][0]=.09));await sleep(30);
  await page.locator('#pj-cancel').click();await page.waitForFunction(()=>!configBusy);await sleep(180);
  assert.equal(history.at(-1).path,'/config/revert');assert.equal(current.outputs[0].geometry.corners[0][0],.08);mark('Save and Revert barriers prevent late previews');
  delay=0;
  const beforePattern=structuredClone(current.outputs[0].geometry);
  await page.locator('#tp-pattern').selectOption('grid');await settle();await page.locator('#pj-save').click();await page.waitForFunction(()=>!configBusy);
  assert.equal(current.projection.testPattern,null);assert.equal(current.projection.mode,'warp');assert.deepEqual(current.outputs[0].geometry,beforePattern);mark('pattern switching and Save preserve geometry and mode');
  rejection=true;await page.evaluate(()=>editGeometry(g=>g.corners[0][0]=.11));await settle();
  assert.deepEqual((await read()).outputs[0].geometry,beforePattern);assert.match(await page.locator('#warp-error').textContent(),/rejects/);mark('server validation rejection rolls back to last accepted geometry');
  await page.evaluate(()=>{projectionStats.geometry.warpAvailable=false;projectionStats.geometry.effectiveMode='simple';projectionStats.geometry.reason='CPU fallback';renderWarpEditor();});
  assert(await page.locator('#mode-warp').isDisabled());
  assert.equal(await page.locator('#mode-simple').getAttribute('aria-pressed'),'true');
  const fallbackCrop=structuredClone(current.outputs[0].geometry.source);
  await page.locator('#source-x').fill('12');await page.locator('#source-x').press('Tab');await settle();
  assert.deepEqual(current.outputs[0].geometry.corners,beforePattern.corners);
  assert.notDeepEqual(current.outputs[0].geometry.source,fallbackCrop);
  assert.equal(current.projection.mode,'warp');
  await page.evaluate(()=>{projectionStats.geometry.warpAvailable=true;projectionStats.geometry.effectiveMode='warp';renderWarpEditor();});
  assert.deepEqual(current.outputs[0].geometry.corners,beforePattern.corners);mark('CPU fallback keeps shared crop editable and preserves requested Warp/correction');
  await page.locator('#mode-simple').click();await settle();
  assert.equal(current.projection.mode,'simple');
  await page.locator('#mode-warp').click();await settle();
  assert.equal(current.projection.mode,'warp');mark('deliberate mode selection changes intent without resetting calibration');
  recommendationDelay=160;
  await page.evaluate(()=>editGeometry(g=>g.corners[0][0]=.04));
  await settle();await sleep(270);
  await page.evaluate(()=>editGeometry(g=>g.corners[0][0]=.045));
  await settle();await sleep(180);
  assert.equal(await page.evaluate(()=>recommendation),null);mark('stale recommendation cannot replace a newer edit');
  recommendationDelay=0;
  await page.waitForFunction(()=>recommendation!=null);
  const oldWidth=current.projection.canvas.renderWidth;
  assert.equal((await read()).projection.canvas.renderWidth,oldWidth);
  const oldMetadata=current.projection.canvas.scale;
  await page.locator('#canvas-width').focus();await page.waitForSelector('#canvas-presets:not([hidden]) button');
  await page.locator('#canvas-presets button').filter({hasText:'50%'}).click();
  await page.waitForFunction(()=>config.projection.canvas.renderWidth===4000);await settle();
  assert.equal(current.projection.canvas.renderWidth,4000);assert.equal(current.projection.canvas.scale,oldMetadata);mark('automatic recommendation is read-only until explicit width preset');
  // --- one shared working copy: the server pushes every change as an event,
  // and there is nothing left to "conflict" over between two clients.
  const second=await context.newPage();await second.goto('http://suede.test/?nosse');await second.waitForFunction(()=>configGeneration!=null);await second.locator('#output-table tbody tr').first().click();
  await second.evaluate(()=>{
    window.fixtureSources=[];
    window.EventSource=class {
      constructor(){this.listeners={};fixtureSources.push(this);}
      addEventListener(name,fn){this.listeners[name]=fn;}
      close(){this.closed=true;}
    };
    connect();
  });
  // Simulates the server push a real daemon makes on every effective-document
  // change (B3); this fixture has no live SSE transport of its own, so the
  // test drives the same `config_changed` listener the real EventSource
  // would, with the payload shape B3 defines.
  const sendConfigChanged=(target,doc,section='outputs')=>{
    const payload={revision:doc.revision,generation,epoch,committed:doc.committed,section,config:doc};
    return target.evaluate(payload=>fixtureSources[0].listeners.config_changed({data:JSON.stringify(payload)}),payload);
  };
  await page.evaluate(()=>editGeometry(g=>g.corners[0][0]=.21));await settle();
  await sendConfigChanged(second,current);
  assert.equal(await second.evaluate(()=>config.outputs[0].geometry.corners[0][0]),.21);
  assert.equal(await second.evaluate(()=>document.getElementById('pj-save').classList.contains('dirty')),true);
  assert(String(await second.locator('#warp-quad').getAttribute('points')).startsWith('0.21,'));
  mark('one client\'s preview arrives on another page as an event and updates fields, dirty state and the diagram');

  // While `#canvas-width` is focused, an event must not yank the operator's
  // half-typed digits out from under them (it still applies to `config`
  // underneath — see `applyLoadedConfig` — just not to this one field's
  // displayed text). Once they blur, the field is no longer protected: this
  // one has an onchange handler like every other editable field, so what
  // lands is the operator's own completed edit — not silently discarded by
  // the event that arrived mid-keystroke, and not silently overwritten by
  // it either.
  await second.locator('#canvas-width').focus();await second.locator('#canvas-width').fill('4321');
  const remoteWidth=structuredClone(current);remoteWidth.projection.canvas.renderWidth=5000;
  await sendConfigChanged(second,remoteWidth);
  assert.equal(await second.locator('#canvas-width').inputValue(),'4321');
  await second.locator('#canvas-width').blur();
  await second.waitForFunction(()=>config.projection.canvas.renderWidth===4321);
  await second.evaluate(()=>flushPreviews());
  mark('a field being typed into is not overwritten by an event until it blurs');

  await page.locator('#pj-cancel').click();await page.waitForFunction(()=>!configBusy);
  await sendConfigChanged(second,current,'revert');
  assert.equal(await second.evaluate(()=>config.committed),true);
  assert.equal(await second.evaluate(()=>document.getElementById('pj-save').classList.contains('dirty')),false);
  mark('a remote revert clears dirty state on another client');

  await page.evaluate(()=>{projectionStats.control={session:'new-session',configGeneration:{applied:Number(configGeneration)-1}};renderWarpStatus();});
  assert.doesNotMatch(await page.locator('#warp-status').textContent(),/Effective projection installed/);
  await page.evaluate(()=>{projectionStats.control.configGeneration.applied=Number(configGeneration);renderWarpStatus();});
  assert.match(await page.locator('#warp-status').textContent(),/Effective projection installed/);mark('installation status uses working-copy correlation, not acceptance alone');
  await page.evaluate(()=>{
    window.fixtureSources=[];
    window.EventSource=class {
      constructor(){this.listeners={};fixtureSources.push(this);}
      addEventListener(name,fn){this.listeners[name]=fn;}
      close(){this.closed=true;}
    };
    connect();
  });
  epoch="restarted-fixture";
  // X-Config-Epoch detection is kept for projection-status correlation (a
  // restarted daemon reuses small numeric generations, and `configGeneration`
  // above is compared against `projectionStats.control.configGeneration`) —
  // not for a conflict banner, which no longer exists. `loadConfig` applies
  // the fetched document unconditionally either way.
  await page.evaluate(()=>loadAll());
  assert(await page.evaluate(()=>fixtureSources[0].closed));
  assert.equal(await page.evaluate(()=>fixtureSources.length),2);
  await page.evaluate(()=>fixtureSources[0].listeners.projection_stats_changed({data:JSON.stringify({running:true,control:{session:'stale',configGeneration:{applied:Number(configGeneration)}}})}));
  assert.notEqual(await page.evaluate(()=>projectionStats?.control?.session),'stale');
  assert.equal(await page.evaluate(()=>configEpoch),'restarted-fixture');
  mark('daemon restart epoch still retires the old event source for status correlation');
  await page.locator('#pj-lift-mode').selectOption('adaptive');await settle();
  await page.locator('#pj-lift').fill('0.2');await settle();
  await page.locator('#pj-rise').fill('800');await settle();
  let lift=(await read()).projection.blackLift;
  assert.deepEqual(lift,{mode:'adaptive',level:.2,darkThreshold:.02,brightThreshold:.2,riseMs:800,fallMs:250,slewPerSecond:.1});
  await page.locator('#tp-pattern').selectOption('white');await settle();
  assert.deepEqual((await read()).projection.blackLift,lift);
  await page.locator('#pj-save').click();await page.waitForFunction(()=>config.committed);await settle();
  assert.deepEqual(committed.projection.blackLift,lift);
  assert.equal(committed.projection.testPattern,null);
  await page.locator('#pj-lift').fill('0.3');await settle();
  await page.locator('#pj-cancel').click();await page.waitForFunction(()=>config.projection.blackLift.level===.2);
  assert.deepEqual((await read()).projection.blackLift,lift);
  mark('adaptive fields survive pattern preview, Save, and Cancel');
  const writesBeforeStatus=history.length;
  await page.evaluate(()=>{
    projectionStats={running:true,control:{blackLift:{metric:'Fixture metric',available:true,paused:false,reason:null,captureId:12,sampleCount:65536,luminance:.15,sampleAgeMs:250,target:.06,applied:.1,logicalGeneration:33}}};
    renderBlackLiftStatus();
  });
  assert.match(await page.locator('#pj-lift-status').textContent(),/capture 12/);
  assert.match(await page.locator('#pj-lift-status').textContent(),/Applied 0\.1(?:0*\b)/);
  await page.evaluate(()=>{projectionStats.control.blackLift={...projectionStats.control.blackLift,available:false,reason:'Measurement unavailable'};renderBlackLiftStatus();});
  assert.match(await page.locator('#pj-lift-status').textContent(),/Adaptive unavailable: fixed compensation/);
  await page.evaluate(()=>{projectionStats.control.blackLift={...projectionStats.control.blackLift,stale:true};renderBlackLiftStatus();});
  assert.match(await page.locator('#pj-lift-status').textContent(),/retaining the last valid target/);
  await page.evaluate(()=>{projectionStats.control.blackLift={...projectionStats.control.blackLift,paused:true};renderBlackLiftStatus();});
  assert.match(await page.locator('#pj-lift-status').textContent(),/Calibration: fixed compensation/);
  await sleep(1100);assert.equal(history.length,writesBeforeStatus);
  mark('adaptive telemetry and age updates never send configuration previews');
  await page.locator('#pj-lift-mode').selectOption('fixed');await settle();
  assert.equal((await read()).projection.blackLift,.2);
  assert.equal(await page.locator('#pj-adaptive-fields').isHidden(),true);
  mark('fixed mode restores numeric configuration semantics');

  assert.deepEqual(failures,[]);
  const output=process.env.UI_EVIDENCE;
  if(output){fs.mkdirSync(output,{recursive:true});await page.locator('#edge-blending-panel').screenshot({path:output+'/editor.png',style:'header, #toast { visibility: hidden !important; }'});fs.writeFileSync(output+'/browser.json',JSON.stringify({browser:browser.version(),measuredAt:new Date().toISOString(),uiSha256:createHash('sha256').update(html).digest('hex'),testSha256:createHash('sha256').update(fs.readFileSync(__filename)).digest('hex'),results,maxFlight,failures},null,2)+'\n');}
  await browser.close();
})().catch(error=>{console.error(error);process.exit(1);});
