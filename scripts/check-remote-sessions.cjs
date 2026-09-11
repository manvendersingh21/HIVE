const { chromium } = require('playwright');
const assert = require('node:assert/strict');
(async () => {
  const browser = await chromium.launch({ executablePath: '/Applications/Google Chrome.app/Contents/MacOS/Google Chrome', headless: true });
  try {
    const page = await browser.newPage();
    const errors = [];
    page.on('pageerror', e => errors.push(e.message));
    await page.goto(process.env.HIVE_TEST_BASE + '/sessions');
    await page.locator('[name=password]').fill('sessions-test-password');
    await page.getByRole('button', { name: 'Sign in', exact: true }).click();
    await page.waitForURL(process.env.HIVE_TEST_BASE + '/');
    await page.goto(process.env.HIVE_TEST_BASE + '/sessions');
    await page.waitForFunction(() => document.querySelectorAll('#machine option').length === 5);
    const name = process.env.HIVE_TEST_SESSION;
    await page.waitForFunction(name => [...document.querySelectorAll('#list .name')].filter(e => e.textContent === name).length === 5, name);
    for (const [host, expected] of [['mac-air', 'Manvenders-MacBook-Air.local'], ['archlinux-worker', 'archlinux'], ['cis-linux2', 'CIS-Linux2'], ['cis-a6000', 'cis-a6000']]) {
      const link = page.locator(`a.open[href="/terminal/${name}?host=${host}"]`);
      await link.click();
      await page.waitForFunction(() => document.querySelector('#state').textContent === 'live');
      // Wait for tmux to draw before sending input, then require command output.
      await page.waitForFunction(() => term.buffer.active.length > 0 && [...Array(term.buffer.active.length).keys()].some(i => term.buffer.active.getLine(i)?.translateToString().trim()));
      await page.evaluate(() => send("printf '\\nSESSION_HOST=%s\\n' \"$(uname -n)\"\r"));
      await page.waitForFunction(expected => [...Array(term.buffer.active.length).keys()].some(i => term.buffer.active.getLine(i)?.translateToString().includes('SESSION_HOST=' + expected)), expected);
      await page.reload();
      await page.waitForFunction(() => document.querySelector('#state').textContent === 'live');
      await page.waitForFunction(expected => [...Array(term.buffer.active.length).keys()].some(i => term.buffer.active.getLine(i)?.translateToString().includes('SESSION_HOST=' + expected)), expected);
      await page.goto(process.env.HIVE_TEST_BASE + '/sessions');
      await page.locator(`a.open[href="/terminal/${name}?host=${host}"]`).waitFor();
      console.log(`PASS: browser terminal and reattach on ${host} (${expected})`);
    }
    assert.deepEqual(errors, []);
  } finally { await browser.close(); }
})().catch(e => { console.error(e); process.exitCode = 1; });
