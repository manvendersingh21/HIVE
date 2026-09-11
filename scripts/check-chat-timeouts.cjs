// Exercise the actual browser request helper without a browser dependency.
const assert = require('node:assert/strict');
const fs = require('node:fs');
const vm = require('node:vm');
const path = require('node:path');
const html = fs.readFileSync(path.join(__dirname, '../hive-web/static/chat.html'), 'utf8');
const helper = html.slice(html.indexOf('  async function chatRequest('), html.indexOf('  function wireGates('));

function setup(fetch) {
  let expire;
  let cleared = false;
  const location = {};
  const context = vm.createContext({
    fetch, AbortController, TypeError, location,
    setTimeout: (fn, delay) => { assert.equal(delay, 180000); expire = fn; return 1; },
    clearTimeout: id => { assert.equal(id, 1); cleared = true; }
  });
  vm.runInContext(helper, context);
  return { request: context.chatRequest, expire: () => expire(), cleared: () => cleared, location };
}

async function main() {
  const ok = setup(async (url, options) => {
    assert.equal(url, '/api/chat');
    assert.deepEqual(JSON.parse(options.body), { message: 'hey' });
    return { ok: true, status: 200, json: async () => ({ result: 'done' }) };
  });
  assert.deepEqual(await ok.request('/api/chat', { message: 'hey' }), { result: 'done' });
  assert(ok.cleared());

  for (const stalledBody of [false, true]) {
    let calls = 0;
    const hung = setup(async (_, { signal }) => {
      calls++;
      const pending = new Promise((resolve, reject) => signal.addEventListener('abort', () => reject(new Error('aborted'))));
      return stalledBody ? { ok: true, status: 200, json: () => pending } : pending;
    });
    const pending = hung.request('/api/chat/run/approve', { approved: [0] });
    await Promise.resolve();
    hung.expire();
    await assert.rejects(pending, /Work may still be running; check Sessions before retrying/);
    assert.equal(calls, 1, 'Never retry a potentially executed command');
    assert(hung.cleared());
  }

  const offline = setup(async () => { throw new TypeError('Failed to fetch'); });
  await assert.rejects(offline.request('/api/chat', {}), /Connection to Hive was lost/);
  assert(offline.cleared());

  const expired = setup(async () => ({ status: 401 }));
  await assert.rejects(expired.request('/api/chat', {}), /session expired/);
  assert.equal(expired.location.href, '/login');
  assert(expired.cleared());

  const failed = setup(async () => ({ ok: false, status: 504, text: async () => 'Planning timed out. No commands were executed.' }));
  await assert.rejects(failed.request('/api/chat', {}), /Planning timed out/);
  assert(failed.cleared());
  console.log('Chat request checks passed: success, stalled headers/body, network failure, session expiry, server timeout, and no automatic retry.');
}
main().catch(error => { console.error(error); process.exitCode = 1; });
