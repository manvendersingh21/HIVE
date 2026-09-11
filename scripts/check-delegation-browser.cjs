#!/usr/bin/env node
// Read-only live checks plus isolated browser fixtures. Never decides an approval
// or sends guidance to the real API. Require Playwright through NODE_PATH.
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const { chromium } = require('playwright');
const baseURL = process.env.HIVE_TEST_URL || 'http://127.0.0.1:18190';
const conversation = process.env.HIVE_TEST_CONVERSATION;
const wantedRuns = (process.env.HIVE_TEST_RUNS || '').split(',').filter(Boolean);
const chrome = process.env.HIVE_TEST_CHROME || '/Applications/Google Chrome.app/Contents/MacOS/Google Chrome';
const fixtureRun = {
  id: 'fixture-run', task_id: 'fixture-task', conversation_id: 'fixture-chat',
  tmux_name: 'hive-agent-fixture', state: 'working', runner_path: '/private/runner.py', cursor: 12,
  assignment: {device: 'fixture-device', agent: 'codex', model: 'requested-model',
    workspace: '~/hive-workspaces/fixture', objective: 'Verify <script> literal text'},
  metadata: {actual_model: 'actual-native-model', native_id: 'native-conversation', approvals: []},
};
const pending = {id: 'fixture-approval', fingerprint: 'immutable-fingerprint', consumed: 0,
  decision: null, action: '{"tool":"item/fileChange/requestApproval","arguments":{"itemId":"change-1"}}',
  reason: 'Outside the assigned workspace', details: {id:'change-1', changes:[{
    path:'/outside/project/file with spaces.txt', diff:'-before\n+<img src=x onerror=alert(1)>',
  }]}};

async function fixtures(browser) {
  const context = await browser.newContext({baseURL});
  let runReads = 0, recoveryMode = false, recoveryBlocked = false;
  const submitted = [], recoveries = [];
  const recovery = {fingerprint:'a'.repeat(64), acknowledge_ids:['delivery-1','delivery-2'],
    snapshot:{last_action:'<img src=x onerror=alert(1)>', native_conversation_id:'native-conversation',
      deliveries:[{id:'delivery-1',state:'delivering'},{id:'delivery-2',state:'uncertain'}]}};
  await context.route('**/api/**', async route => {
    const req=route.request(), url=new URL(req.url()), path=url.pathname;
    let body;
    if (path==='/api/capabilities') body={chat:true};
    else if (path==='/api/chats') body={chats:[], next_cursor:null};
    else if (path==='/api/chats/fixture-chat') body={chat:{id:'fixture-chat',title:'Fixture'},messages:[{
      role:'assistant',status:'completed',content:'Delegated',reply:{delegation:{task_id:'fixture-task',summary:'Fixture delegation',runs:[fixtureRun]}}
    }]};
    else if (path==='/api/runs') {
      runReads++;
      body=[{...fixtureRun,state:recoveryMode?'failed':runReads===1?'working':'awaiting-approval',metadata:{...fixtureRun.metadata,approvals:runReads===1?[]:[pending]}}];
    } else if (path==='/api/runs/fixture-run/events') body=[{payload:{text:'Live fixture output <script> literal'}}];
    else if (path==='/api/runs/fixture-run/decisions') {
      assert.equal(req.method(),'POST'); submitted.push(req.postDataJSON()); body={saved:true};
    } else if (path==='/api/runs/fixture-run/recovery') {
      if(req.method()==='POST'){recoveries.push(req.postDataJSON());body={saved:true};}
      else {assert.equal(req.method(),'GET');body={...recovery,can_resume:!recoveryBlocked,blockers:recoveryBlocked?['Native runner is still active <script>']:[]};}
    } else if (path==='/api/session-hosts') body=[];
    else if (path==='/api/sessions') body=[{name:fixtureRun.tmux_name,host:'fixture-device',windows:1,created:Math.floor(Date.now()/1000),attached:false,current_command:'python3',window_name:'codex',run:fixtureRun}];
    else throw new Error(`Unmocked API request: ${req.method()} ${path}`);
    await route.fulfill({status:200,contentType:'application/json',body:JSON.stringify(body)});
  });
  for (const [routePath, file] of [['chat.html','chat.html'], ['sessions','index.html']]) {
    await context.route(url=>url.pathname===`/${routePath}`, route=>route.fulfill({status:200,contentType:'text/html',body:fs.readFileSync(path.join(__dirname,'../hive-web/static',file),'utf8')}));
  }
  const page=await context.newPage();
  const errors=[];page.on('pageerror',error=>errors.push(error.message));
  await page.goto('/chat.html#chat=fixture-chat');
  await page.locator('[data-output="fixture-run"]').waitFor();
  assert.equal(await page.locator('#input').isEnabled(),true,'Composer remains available during native work');
  assert.equal(await page.locator('a:has-text("Open Session")').getAttribute('href'),'/terminal/hive-agent-fixture?host=fixture-device');
  await page.locator('[data-output="fixture-run"]').click();
  await page.waitForFunction(()=>document.querySelector('[data-run-output="fixture-run"]').textContent.includes('Live fixture output'));
  // The run changes after the first read; polling must display the new approval
  // while the composer is free. No browser reload should be necessary.
  await page.locator('[data-decision="continue"]').waitFor({timeout:10000});
  assert.equal(await page.locator('[data-run-output="fixture-run"]').isVisible(),true,'Output expansion survives refresh');
  const gate=page.locator('.gate');
  const detail=await gate.textContent();
  assert.ok(detail.includes('/outside/project/file with spaces.txt'));
  assert.ok(detail.includes('<img src=x onerror=alert(1)>'));
  assert.equal(await gate.locator('img').count(),0,'Approval details are escaped');
  assert.equal(await page.locator('#input').isEnabled(),true);
  await page.reload();
  await page.locator('[data-decision="continue"]').waitFor();
  assert.equal(await page.locator('[data-decision="continue"]').getAttribute('data-fingerprint'),pending.fingerprint);
  // Intercepted entirely by the fixture above; this cannot approve a live run.
  await page.locator('[data-decision="continue"]').click();
  await page.waitForFunction(()=>!document.querySelector('[data-decision="continue"]')?.disabled);
  assert.deepEqual(submitted,[{id:pending.id,fingerprint:pending.fingerprint,decision:'continue'}]);
  recoveryMode=true;
  await page.reload();
  await page.locator('[data-recovery="fixture-run"]').waitFor();
  assert.equal(await page.locator('#input').isEnabled(),true,'Failed native work leaves chat available');
  await page.locator('[data-recovery="fixture-run"]').click();
  let dialog=page.locator('dialog');
  await dialog.waitFor();
  await dialog.locator('summary').click();
  assert.ok((await dialog.locator('pre').textContent()).includes('<img src=x onerror=alert(1)>'));
  assert.equal(await dialog.locator('img').count(),0,'Recorded actions are escaped');
  await dialog.locator('[data-resume]').click();
  assert.ok((await dialog.locator('[data-recovery-error]').textContent()).includes('acknowledge every uncertain delivery'));
  assert.equal(recoveries.length,0,'Empty recovery evidence cannot be submitted');
  await dialog.locator('[data-reason]').fill(' Native stream interrupted ');
  await dialog.locator('[data-evidence]').fill(' Checked the last tool result and file hashes; no uncertain action will be replayed. ');
  await dialog.locator('[data-ack="delivery-1"]').check();
  await dialog.locator('[data-resume]').click();
  assert.equal(recoveries.length,0,'Every uncertain delivery must be acknowledged');
  await dialog.locator('[data-ack="delivery-2"]').check();
  await dialog.locator('[data-resume]').click();
  await dialog.waitFor({state:'detached'});
  assert.deepEqual(recoveries,[{fingerprint:recovery.fingerprint,reason:'Native stream interrupted',
    evidence:'Checked the last tool result and file hashes; no uncertain action will be replayed.',acknowledge_ids:['delivery-1','delivery-2']}]);
  assert.equal(await page.locator('#input').isEnabled(),true);
  recoveryBlocked=true;
  await page.locator('[data-recovery="fixture-run"]').click();
  dialog=page.locator('dialog');
  await dialog.waitFor();
  assert.equal(await dialog.locator('[data-resume]').isDisabled(),true,'Server blockers disable resumption');
  assert.ok((await dialog.textContent()).includes('Native runner is still active <script>'));
  assert.equal(await dialog.locator('script').count(),0,'Recovery blockers are escaped');
  await dialog.locator('[data-close]').click();
  assert.equal(recoveries.length,1,'Blocked recovery sends no request');
  assert.equal(await page.locator('#input').isEnabled(),true);
  await page.goto('/sessions');
  await page.locator('#list a.open').waitFor();
  assert.ok((await page.locator('#list').textContent()).includes('actual-native-model'));
  assert.equal(await page.locator('#list a.open').getAttribute('href'),'/terminal/hive-agent-fixture?host=fixture-device');
  assert.deepEqual(errors,[]);
  await context.close();
  console.log('PASS browser fixtures: live polling, composer, escaped approval details, exact decision and recovery payloads, recovery acknowledgments/blockers, reload, session metadata');
}

async function live(browser) {
  assert.ok(process.env.HIVE_TEST_PASSWORD,'HIVE_TEST_PASSWORD is required for live checks');
  assert.ok(conversation,'HIVE_TEST_CONVERSATION is required for live checks');
  const context=await browser.newContext({baseURL});
  const login=await context.request.post('/login',{form:{password:process.env.HIVE_TEST_PASSWORD}});
  assert.ok(login.ok(),`Login failed (${login.status()})`);
  const read=async path=>{const response=await context.request.get(path);assert.ok(response.ok(),`${path}: HTTP ${response.status()}`);return response.json();};
  const initial=await read(`/api/runs?conversation_id=${encodeURIComponent(conversation)}`);
  const runs=initial.filter(run=>wantedRuns.length?wantedRuns.includes(run.id):run.state!=='superseded');
  assert.ok(runs.length>=2,'Need at least two existing runs for separate session checks');
  if(wantedRuns.length)assert.equal(runs.length,wantedRuns.length);
  assert.equal(new Set(runs.map(run=>`${run.assignment.device}/${run.tmux_name}`)).size,runs.length);
  const page=await context.newPage();
  const errors=[];page.on('pageerror',error=>errors.push(error.message));
  await page.goto(`/chat.html#chat=${encodeURIComponent(conversation)}`);
  for(const run of runs){
    await page.locator(`[data-output="${run.id}"]`).waitFor();
    const card=page.locator('.step').filter({has:page.locator(`[data-output="${run.id}"]`)});
    assert.equal(await card.getByRole('link',{name:'Open Session'}).getAttribute('href'),`/terminal/${encodeURIComponent(run.tmux_name)}?host=${encodeURIComponent(run.assignment.device)}`);
    await page.locator(`[data-output="${run.id}"]`).click();
    await page.waitForFunction(id=>!document.querySelector(`[data-run-output="${id}"]`).hidden,run.id);
    const events=await read(`/api/runs/${run.id}/events?after=${Math.max(0,run.cursor-100)}`);
    assert.ok(Array.isArray(events));
  }
  assert.equal(await page.locator('#input').isEnabled(),true);
  await page.reload();
  for(const run of runs)await page.locator(`[data-output="${run.id}"]`).waitFor();
  assert.equal(await page.locator('#input').isEnabled(),true);
  const after=await read(`/api/runs?conversation_id=${encodeURIComponent(conversation)}`);
  for(const run of runs){
    const resumed=after.find(value=>value.id===run.id);assert.ok(resumed);
    assert.equal(resumed.tmux_name,run.tmux_name);
    if(run.metadata.native_conversation_id)assert.equal(resumed.metadata.native_conversation_id,run.metadata.native_conversation_id);
  }
  const sessions=await read('/api/sessions');
  await page.goto('/sessions');
  for(const run of runs){
    const session=sessions.find(value=>value.host===run.assignment.device&&value.name===run.tmux_name);
    assert.ok(session,`Missing session metadata for ${run.id}`);assert.equal(session.run.id,run.id);
    await page.locator(`a.open[href="/terminal/${encodeURIComponent(run.tmux_name)}?host=${encodeURIComponent(run.assignment.device)}"]`).waitFor();
    // Fetch the page without starting an interactive WebSocket or writing input.
    const response=await context.request.get(`/terminal/${encodeURIComponent(run.tmux_name)}?host=${encodeURIComponent(run.assignment.device)}`);
    assert.ok(response.ok());assert.ok((await response.text()).includes('new Terminal('));
  }
  assert.deepEqual(errors,[]);
  await context.close();
  console.log(`PASS live read-only: ${runs.length} distinct session pages, metadata, event API, available chat, stable identities across browser reload`);
}

(async()=>{
  const browser=await chromium.launch({headless:true,executablePath:chrome});
  try {if(process.argv.includes('--live-only'))await live(browser);else{await fixtures(browser);if(process.argv.includes('--live'))await live(browser);}}
  finally {await browser.close();}
})().catch(error=>{console.error(error.message);process.exitCode=1;});
